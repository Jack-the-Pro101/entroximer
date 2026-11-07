import { parseDuration } from "entroximer-client";
import type { QueueItem, Track } from "entroximer-client";

/** `m:ss`, or `h:mm:ss` from an hour. */
export function formatTime(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1000));
  const [hours, minutes, seconds] = [Math.floor(total / 3600), Math.floor(total / 60) % 60, total % 60];
  const ss = String(seconds).padStart(2, "0");
  return hours > 0 ? `${hours}:${String(minutes).padStart(2, "0")}:${ss}` : `${minutes}:${ss}`;
}

/** Parses `90`, `1:30`, `1:02:03` or `1m30s` into milliseconds; `null` if malformed. */
export function parseTime(text: string): number | null {
  return parseDuration(text);
}

/**
 * The part of a track to play, as `/play` and `/trim` take it: a side left out stays as the link
 * or the track has it; `null` is the track's beginning or end.
 */
export interface TimeRange {
  startTime?: number | null;
  endTime?: number | null;
}

/** How to write a `TimeRange`, for when one does not parse. */
export const TIME_RANGE_HELP =
  "Give the time as `1:30` (from), `1:30-2:45` (from and to) or `-2:45` (to), in seconds, `m:ss` or `1m30s`; `0` is the beginning and `end` the end.";

/**
 * Parses `1:30` (from there), `1:30-2:45` (from and to) or `-2:45` (to there) into milliseconds;
 * `start` (or `0`) and `end` stand for the track's own beginning and end. `null` if malformed.
 */
export function parseTimeRange(text: string): TimeRange | null {
  const sides = text.split(/\s*(?:-|–|\bto\b)\s*/).map((side) => side.trim().toLowerCase());
  const [start = "", end = ""] = sides;
  if (sides.length > 2 || (start === "" && end === "")) {
    return null;
  }
  /** A side's time: `undefined` if left out, `null` for `word`, `NaN` if malformed. */
  const read = (side: string, word: string) =>
    side === "" ? undefined : side === word ? null : (parseTime(side) ?? Number.NaN);
  const startTime = read(start, "start");
  const endTime = read(end, "end");
  if (Number.isNaN(startTime) || Number.isNaN(endTime)) {
    return null;
  }
  return {
    ...(startTime === undefined ? {} : { startTime }),
    ...(endTime === undefined ? {} : { endTime }),
  };
}

/** `3:45`, `1:30–2:45 of 3:45` for part of it, `live` or `unknown length`. */
export function trackLength(track: Track): string {
  const { length, isStream } = track.info;
  if (isStream) {
    return "live";
  }
  const whole = length === null ? "unknown length" : formatTime(length);
  const { startTime, endTime } = track;
  if (startTime === undefined && endTime === undefined) {
    return whole;
  }
  const end = endTime ?? length;
  const from = formatTime(startTime ?? 0);
  const part = end === null ? `from ${from}` : `${from}–${formatTime(end)}`;
  return length === null ? part : `${part} of ${whole}`;
}

/** How long the part of the track that plays lasts; `null` if unknown. */
export function playedLength(track: Track): number | null {
  const end = track.endTime ?? track.info.length;
  return end === null ? null : Math.max(0, end - (track.startTime ?? 0));
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
