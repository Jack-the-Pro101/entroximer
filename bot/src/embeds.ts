// Embeds for what is playing, the queue and search results, each track with its artwork.
import { AttachmentBuilder, EmbedBuilder } from "discord.js";
import type { Entroximer, Player, Track } from "entroximer-client";

import { describeItem, describeTrack, escape, formatTime, playedLength, trackLength, truncate } from "./format";

export const COLOR = 0x2bb3a0;
/** Queue items per page of `/queue`: a message holds ten embeds, one of them the header. */
export const QUEUE_PAGE = 9;

/** Embeds, and the artwork they upload, ready to send. */
export interface EmbedMessage {
  embeds: EmbedBuilder[];
  files: AttachmentBuilder[];
}

/**
 * How an embed shows `track`'s artwork: by its URL when Discord can fetch it, else with the image
 * itself uploaded as `name` (covers the engine extracted from media files are on the engine,
 * which Discord cannot reach).
 */
async function artwork(manager: Entroximer, track: Track, name: string): Promise<{ url: string | null; file?: AttachmentBuilder }> {
  const location = manager.artwork(track);
  if (location === null) {
    return { url: null };
  }
  if (location.public) {
    return { url: location.url };
  }
  const image = await manager.fetchArtwork(track);
  if (image === null) {
    return { url: null };
  }
  const file = new AttachmentBuilder(Buffer.from(await image.arrayBuffer()), { name });
  return { url: `attachment://${name}`, file };
}

/** One embed per track, each with its artwork; `describe` writes its text. */
async function trackCards(
  manager: Entroximer,
  tracks: readonly Track[],
  describe: (track: Track, index: number) => string,
): Promise<EmbedMessage> {
  const cards = await Promise.all(
    tracks.map(async (track, index) => {
      const art = await artwork(manager, track, `artwork-${index}.jpg`);
      const embed = new EmbedBuilder().setColor(COLOR).setDescription(truncate(describe(track, index), 4096));
      return { embed: embed.setThumbnail(art.url), file: art.file };
    }),
  );
  return {
    embeds: cards.map((card) => card.embed),
    files: cards.flatMap((card) => (card.file ? [card.file] : [])),
  };
}

export async function nowPlayingMessage(manager: Entroximer, player: Player, track: Track): Promise<EmbedMessage> {
  const { title, uri, author, length, isStream } = track.info;
  const position = player.position;
  const art = await artwork(manager, track, "artwork.jpg");
  const embed = new EmbedBuilder()
    .setColor(COLOR)
    .setAuthor({ name: player.paused ? "Paused" : "Now playing" })
    .setTitle(truncate(title, 256))
    .setThumbnail(art.url);
  if (/^https?:\/\//.test(uri)) {
    embed.setURL(uri);
  }
  if (author) {
    embed.setDescription(`by ${escape(author)}`);
  }
  // the bar spans the part that plays
  const start = track.startTime ?? 0;
  const end = track.endTime ?? length;
  const part = start > 0 || track.endTime !== undefined ? ` (playing ${trackLength(track)})` : "";
  const progress =
    isStream || end === null
      ? `${formatTime(position)}${part}`
      : `${progressBar((position - start) / Math.max(1, end - start))} ${formatTime(position)} / ${formatTime(end)}${part}`;
  embed.addFields({ name: isStream ? "Live for" : "Position", value: progress });

  const next = player.queue.items[0];
  embed.addFields({
    name: "Up next",
    value: next ? truncate(describeItem(next), 1024) : "Nothing queued",
  });
  const buffered = player.state?.buffered ?? [];
  const details = [
    `volume ${player.volume}%${player.clipping ? " (clipping)" : ""}`,
    `repeat ${player.queue.repeat}`,
    ...(player.state?.buffering ? ["buffering"] : []),
    ...(buffered.length > 0
      ? [`downloaded ${buffered.map(([start, end]) => `${formatTime(start)}–${formatTime(end)}`).join(", ")}`]
      : []),
  ];
  embed.setFooter({ text: details.join(" · ") });
  return { embeds: [embed], files: art.file ? [art.file] : [] };
}

/** Page `page` (from 1) of the queue, a card with artwork per track; pages past the end show the last one. */
export async function queueMessage(manager: Entroximer, player: Player, page: number): Promise<EmbedMessage> {
  const items = player.queue.items;
  const pages = Math.max(1, Math.ceil(items.length / QUEUE_PAGE));
  const shown = Math.min(Math.max(1, page), pages);
  const first = (shown - 1) * QUEUE_PAGE;
  const onPage = items.slice(first, first + QUEUE_PAGE);

  const header = new EmbedBuilder().setColor(COLOR).setTitle("Queue");
  const files: AttachmentBuilder[] = [];
  if (player.track) {
    const art = await artwork(manager, player.track, "now-playing.jpg");
    header.addFields({ name: "Now playing", value: truncate(describeTrack(player.track), 1024) }).setThumbnail(art.url);
    files.push(...(art.file ? [art.file] : []));
  }
  if (onPage.length === 0) {
    header.setDescription("Nothing queued.");
  }
  const lengths = items.map((item) => playedLength(item.track));
  const known = lengths.filter((length) => length !== null);
  const total = known.reduce((sum, length) => sum + length, 0);
  const duration = `${formatTime(total)}${known.length < items.length ? "+" : ""}`;
  const loading = player.queue.loading;
  const count = `${items.length} tracks${loading > 0 ? ` (${loading} loading)` : ""}`;
  header.setFooter({
    text: `Page ${shown} of ${pages} · ${count} · ${duration} · repeat ${player.queue.repeat}`,
  });

  // loading items have no artwork yet, so nothing is fetched for them
  const cards = await trackCards(
    manager,
    onPage.map((item) => item.track),
    (_, index) => `${first + index + 1}. ${onPage[index] ? describeItem(onPage[index]) : ""}`,
  );
  return { embeds: [header, ...cards.embeds], files: [...files, ...cards.files] };
}

/** Search results, a card with artwork per track (at most ten, as a message holds). */
export async function searchMessage(manager: Entroximer, tracks: readonly Track[], source: string): Promise<EmbedMessage> {
  const message = await trackCards(manager, tracks.slice(0, 10), (track, index) => `${index + 1}. ${describeTrack(track)}`);
  message.embeds[0]?.setTitle(`Results from ${source}`);
  return message;
}

/** `▬▬▬🔘▬▬▬▬▬▬` for a fraction from 0 to 1. */
function progressBar(fraction: number): string {
  const width = 12;
  const at = Math.min(width - 1, Math.max(0, Math.floor(fraction * width)));
  return `${"▬".repeat(at)}🔘${"▬".repeat(width - 1 - at)}`;
}
