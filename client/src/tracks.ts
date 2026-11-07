import type { Track } from "./generated/Track.js";
import type { TrackSelector } from "./generated/TrackSelector.js";
import type { JsonValue } from "./generated/serde_json/JsonValue.js";
import { parseLinkTimes } from "./times.js";
import { whole } from "./units.js";

/**
 * A track to play: one from `load` or `search`, or a URL, search (`ytsearch:...`) or path for
 * the engine to load.
 */
export type Playable = Track | string;

/** How a selector names its track, beyond the track itself. */
export interface SelectorOptions {
  /** Replaces the track's own. */
  userData?: JsonValue | undefined;
  /** The index of the track a playlist query starts from. */
  startIndex?: number | undefined;
  /** Replaces the link's or the track's own start time; `null` (or 0) plays from the beginning. */
  startTime?: number | null | undefined;
  /** Replaces the link's or the track's own end time; `null` plays to the end. */
  endTime?: number | null | undefined;
}

/**
 * How the protocol names `track`. A query's link gives its times (see `parseLinkTimes`), a
 * track has its own; the options' replace both.
 */
export function selector(track: Playable, options: SelectorOptions = {}): TrackSelector {
  const data = options.userData ?? (typeof track === "string" ? undefined : track.userData);
  let base: TrackSelector;
  let own: { startTime?: number | undefined; endTime?: number | undefined };
  if (typeof track === "string") {
    const link = parseLinkTimes(track);
    const { startIndex } = options;
    base = { query: link.query, ...(startIndex === undefined ? {} : { startIndex: whole(startIndex) }) };
    own = link;
  } else {
    base = { encoded: track.encoded };
    own = track;
  }
  const startTime = options.startTime === undefined ? own.startTime : options.startTime;
  const endTime = options.endTime === undefined ? own.endTime : options.endTime;
  return {
    ...base,
    ...(startTime == null || startTime <= 0 ? {} : { startTime: whole(startTime) }),
    ...(endTime == null ? {} : { endTime: whole(endTime) }),
    ...(data === undefined ? {} : { userData: data }),
  };
}
