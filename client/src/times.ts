/**
 * Times in a track as people write them, and as links carry them. The engine plays a track from a
 * `startTime` to an `endTime` but never reads them from links: which parameter means what depends
 * on the site, so the client reads them, and `queue.add`, `play` and `load` send them along.
 */

/** A link's start and end times, in milliseconds, and the link without them. */
export interface LinkTimes {
  /** The link with the time parameters read taken out; as it was if it had none. */
  query: string;
  startTime?: number;
  endTime?: number;
}

const CLOCK = /^(\d+)(?::(\d+))?(?::(\d+))?(\.\d+)?$/;
const UNITS = /^(?:(\d+(?:\.\d+)?)\s*h)?\s*(?:(\d+(?:\.\d+)?)\s*m)?\s*(?:(\d+(?:\.\d+)?)\s*s)?$/i;

/**
 * A duration or position in milliseconds, as people and links write them: seconds (`90`, `90.5`,
 * `90s`), units (`1m30s`, `1h2m3s`, `2m`) or a clock (`1:30`, `1:02:03`, `1:30.5`); `null` if it
 * is none of these. A clock's minutes and seconds after the first part go up to 59.
 */
export function parseDuration(text: string): number | null {
  const trimmed = text.trim();
  const clock = CLOCK.exec(trimmed);
  if (clock) {
    const [, first, second, third, fraction] = clock;
    const parts = [first, second, third].filter((part) => part !== undefined).map(Number);
    if (parts.slice(1).some((part) => part > 59)) {
      return null;
    }
    const seconds = parts.reduce((total, part) => total * 60 + part, 0) + Number(fraction ?? 0);
    return Math.round(seconds * 1000);
  }
  const units = UNITS.exec(trimmed);
  if (!units || trimmed === "") {
    return null;
  }
  const [, hours, minutes, seconds] = units.map((unit) => Number(unit ?? 0));
  return Math.round(((hours ?? 0) * 3600 + (minutes ?? 0) * 60 + (seconds ?? 0)) * 1000);
}

/** Where a site puts times in its links: query (or fragment) parameters, by what they mean. */
interface Site {
  hosts: RegExp;
  /** Only links to these paths, e.g. Twitch's videos rather than its channels. */
  path?: RegExp;
  start: string[];
  end?: string[];
  /** Whether the fragment (`#t=90`) holds them too, not only the query. */
  fragment?: boolean;
}

const SITES: Site[] = [
  // `t=90`, `t=1m30s`, `start=` and `end=` (embeds), `time_continue=`; in the fragment too
  {
    hosts: /^((www|m|music)\.)?youtube\.com$|^youtu\.be$|^(www\.)?youtube-nocookie\.com$/,
    start: ["t", "start", "time_continue"],
    end: ["end"],
    fragment: true,
  },
  // `t=1h2m3s` on past broadcasts
  { hosts: /^((www|m)\.)?twitch\.tv$/, path: /^\/videos\/\d+/, start: ["t"] },
  // `start=90`
  { hosts: /^((www|m)\.)?dailymotion\.com$|^dai\.ly$/, start: ["start"] },
  // `t=90`
  { hosts: /^((www|m)\.)?bilibili\.com$/, start: ["t"] },
];

/**
 * The start and end times a link carries, e.g. YouTube's `?t=1m30s`, Twitch's `?t=1h2m3s` or
 * a media fragment (`#t=90`, `#t=1:30,2:45`, as SoundCloud and Vimeo links have), and the link
 * without them. Anything else (searches, links without times, times that do not parse) is
 * returned as it is.
 */
export function parseLinkTimes(query: string): LinkTimes {
  const unchanged = { query };
  let url: URL;
  try {
    url = new URL(query.trim());
  } catch {
    return unchanged;
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    return unchanged;
  }
  const host = url.hostname.toLowerCase();
  const site = SITES.find((s) => s.hosts.test(host) && (s.path?.test(url.pathname) ?? true));
  const search = new Pieces(url.search.slice(1));
  const fragment = new Pieces(url.hash.slice(1));
  let startTime: number | undefined;
  let endTime: number | undefined;
  if (site) {
    for (const pieces of site.fragment ? [search, fragment] : [search]) {
      startTime ??= pieces.take(site.start, parseDuration);
      endTime ??= site.end ? pieces.take(site.end, parseDuration) : undefined;
    }
  }
  if (startTime === undefined && endTime === undefined) {
    const media = fragment.take(["t"], parseMediaFragment);
    ({ startTime, endTime } = media ?? {});
  }
  if (!search.changed && !fragment.changed) {
    return unchanged;
  }
  url.search = search.toString();
  url.hash = fragment.toString();
  return {
    query: url.toString(),
    // `t=0` is the beginning
    ...(startTime === undefined || startTime === 0 ? {} : { startTime }),
    ...(endTime === undefined ? {} : { endTime }),
  };
}

/** A media fragment's time: `start`, `start,end` or `,end`, optionally after `npt:`. */
function parseMediaFragment(value: string): { startTime?: number; endTime?: number } | null {
  const parts = value.replace(/^npt:/, "").split(",");
  const [start = "", end = ""] = parts;
  if (parts.length > 2 || (start === "" && end === "")) {
    return null;
  }
  const startTime = start === "" ? undefined : parseDuration(start);
  const endTime = end === "" ? undefined : parseDuration(end);
  if (startTime === null || endTime === null) {
    return null;
  }
  return { ...(startTime === undefined ? {} : { startTime }), ...(endTime === undefined ? {} : { endTime }) };
}

/**
 * A query's or fragment's `name=value` pieces, kept as written, so that taking one out leaves the
 * others (and their encoding) as they were.
 */
class Pieces {
  readonly #pieces: string[];
  changed = false;

  constructor(text: string) {
    this.#pieces = text === "" ? [] : text.split("&");
  }

  /** Takes out the first of `names` whose value `parse` reads, and returns what it read. */
  take<T>(names: string[], parse: (value: string) => T | null): T | undefined {
    for (const name of names) {
      const index = this.#pieces.findIndex((piece) => piece.split("=", 1)[0] === name);
      const piece = this.#pieces[index];
      if (piece === undefined) {
        continue;
      }
      let value: string;
      try {
        value = decodeURIComponent(piece.slice(name.length + 1).replaceAll("+", " "));
      } catch {
        continue;
      }
      const parsed = parse(value);
      if (parsed !== null) {
        this.#pieces.splice(index, 1);
        this.changed = true;
        return parsed;
      }
    }
    return undefined;
  }

  toString(): string {
    return this.#pieces.join("&");
  }
}
