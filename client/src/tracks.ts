import type { Track } from "./generated/Track.js";
import type { TrackSelector } from "./generated/TrackSelector.js";
import type { JsonValue } from "./generated/serde_json/JsonValue.js";
import { whole } from "./units.js";

/**
 * A track to play: one from `load` or `search`, or a URL, search (`ytsearch:...`) or path for
 * the engine to load.
 */
export type Playable = Track | string;

/**
 * How the protocol names `track`; `userData` replaces the track's own. `startIndex` is the index
 * of the track a playlist query starts from.
 */
export function selector(track: Playable, userData: JsonValue | undefined, startIndex?: number): TrackSelector {
  const data = userData ?? (typeof track === "string" ? undefined : track.userData);
  const base: TrackSelector =
    typeof track === "string"
      ? { query: track, ...(startIndex === undefined ? {} : { startIndex: whole(startIndex) }) }
      : { encoded: track.encoded };
  return data === undefined ? base : { ...base, userData: data };
}
