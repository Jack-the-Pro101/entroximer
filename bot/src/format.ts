import type { QueueItem, Track } from "entroximer-client";

/** `m:ss`, or `h:mm:ss` from an hour. */
export function formatTime(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1000));
  const [hours, minutes, seconds] = [Math.floor(total / 3600), Math.floor(total / 60) % 60, total % 60];
  const ss = String(seconds).padStart(2, "0");
  return hours > 0 ? `${hours}:${String(minutes).padStart(2, "0")}:${ss}` : `${minutes}:${ss}`;
}

/** Parses `90`, `1:30` or `1:02:03` into milliseconds; `null` if malformed. */
export function parseTime(text: string): number | null {
  const parts = text.trim().split(":");
  if (parts.length > 3 || parts.some((part) => !/^\d+(\.\d+)?$/.test(part))) {
    return null;
  }
  const seconds = parts.reduce((total, part) => total * 60 + Number(part), 0);
  return Math.round(seconds * 1000);
}

/** `3:45`, `live` or `unknown length`. */
export function trackLength(track: Track): string {
  const { length, isStream } = track.info;
  return isStream ? "live" : length === null ? "unknown length" : formatTime(length);
}

/** A track as a Markdown link (without an embed), its author and length. */
export function describeTrack(track: Track): string {
  const { title, uri, author } = track.info;
  const linked = /^https?:\/\//.test(uri) ? `[${escape(title)}](<${uri}>)` : escape(title);
  return `**${linked}**${author ? ` by ${escape(author)}` : ""} (${trackLength(track)})`;
}

/** A queue item: its track, or for one still loading, its link (or search) marked as loading. */
export function describeItem(item: QueueItem): string {
  if (!item.loading) {
    return describeTrack(item.track);
  }
  // until it loads, it stands for its link or search
  const { uri } = item.track.info;
  return `⏳ ${/^https?:\/\//.test(uri) ? `<${uri}>` : escape(uri)} (loading)`;
}

/** A track in plain text, for menus and suggestions, which show no Markdown. */
export function plainTrack(track: Track): string {
  const { title, author } = track.info;
  return `${title}${author ? ` — ${author}` : ""} (${trackLength(track)})`;
}

/** Cuts `text` to `max` characters, marking the cut. */
export function truncate(text: string, max: number): string {
  return text.length <= max ? text : `${text.slice(0, max - 1)}…`;
}

/** Escapes Markdown that would otherwise format text in the middle of a line. */
export function escape(text: string): string {
  return text.replace(/([\\*_`~|[\]()<>])/g, "\\$1");
}
