import type { Track } from "./generated/Track.js";
import type { TrackSelector } from "./generated/TrackSelector.js";
import type { JsonValue } from "./generated/serde_json/JsonValue.js";

/**
 * A track to play: one from `load` or `search`, or a URL, search (`ytsearch:...`) or path for
 * the engine to load.
 */
export type Playable = Track | string;

/** How the protocol names `track`; `userData` replaces the track's own. */
export function selector(track: Playable, userData: JsonValue | undefined): TrackSelector {
  const data = userData ?? (typeof track === "string" ? undefined : track.userData);
  const base = typeof track === "string" ? { query: track } : { encoded: track.encoded };
  return data === undefined ? base : { ...base, userData: data };
}
