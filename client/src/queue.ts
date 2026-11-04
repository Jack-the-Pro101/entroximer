import type { Requester } from "./connection.js";
import { Emitter } from "./emitter.js";
import { EntroximerError } from "./errors.js";
import type { PlaylistInfo } from "./generated/PlaylistInfo.js";
import type { QueueAdded } from "./generated/QueueAdded.js";
import type { QueueChange } from "./generated/QueueChange.js";
import type { QueueItem } from "./generated/QueueItem.js";
import type { QueueState } from "./generated/QueueState.js";
import type { Repeat } from "./generated/Repeat.js";
import type { Track } from "./generated/Track.js";
import type { JsonValue } from "./generated/serde_json/JsonValue.js";
import { selector } from "./tracks.js";
import type { Playable } from "./tracks.js";
import { whole } from "./units.js";

/** Items one `queue.get` fetches while syncing. */
const PAGE = 1000;
/** Syncing starts over if the queue changes between its pages, at most this often. */
const SYNC_ATTEMPTS = 5;
/**
 * Outcomes of loads kept for `whenLoaded`, which may be asked after the load finished: the events
 * of a quick one arrive before `add` resolves.
 */
const OUTCOMES_KEPT = 256;

type QueueEvents = {
  /** The queue changed, by anyone's doing; `items` already shows it. */
  change: [change: QueueChange];
  /** `items` was fetched again in full, after changes were missed. */
  sync: [];
};

export interface AddOptions {
  /** Where the tracks go; the end if absent or past it. */
  index?: number;
  /** Data echoed in the tracks' events, e.g. who requested them, in place of their own. */
  userData?: JsonValue;
  /**
   * For a playlist link: the index of the track to start from, counted from 0, in place of the
   * one the link points at; the tracks before it are left out. Applies to every query given.
   */
  startIndex?: number;
}

/** What took the place of a loading item. */
export interface LoadedItems {
  /** The items its query found, the first with the loading item's id. */
  items: QueueItem[];
  /** The playlist they came from, if any. */
  playlist: PlaylistInfo | null;
}

type Outcome = { loaded: LoadedItems } | { error: EntroximerError };

/**
 * A player's queue: the tracks that play after the current one. The engine plays it by itself,
 * loading each next track well before the current one ends so that they follow without a gap;
 * this object mirrors it from the engine's events and fetches it again if any were missed.
 */
export class Queue extends Emitter<QueueEvents> {
  #items: readonly QueueItem[] = [];
  #repeat: Repeat = "off";
  #crossfade = 0;
  #version = 0;
  /** Changes that arrive while syncing, applied on top of the fetched queue. */
  #buffered: Array<{ version: number; change: QueueChange }> | null = null;
  #syncing: Promise<void> | null = null;
  /** Callers of `whenLoaded`, by item id. */
  readonly #waiting = new Map<string, Array<(outcome: Outcome) => void>>();
  /** How recent loads ended, by item id, oldest first. */
  readonly #outcomes = new Map<string, Outcome>();
  /** Why loading items' queries failed, until their removal, which follows. */
  readonly #failures = new Map<string, EntroximerError>();
  readonly #request: Requester;
  readonly #report: (error: unknown) => void;

  /** @internal Created by its player. */
  constructor(
    readonly guildId: string,
    request: Requester,
    report: (error: unknown) => void,
  ) {
    super();
    this.#request = request;
    this.#report = report;
  }

  /** The items in the order they play; replaced, not changed, on every change. */
  get items(): readonly QueueItem[] {
    return this.#items;
  }

  get length(): number {
    return this.#items.length;
  }

  /** `track` plays the current track again and again; `queue` sends finished tracks to the end. */
  get repeat(): Repeat {
    return this.#repeat;
  }

  /** How long the end of each track overlaps the start of the next, in ms; 0 for none. */
  get crossfade(): number {
    return this.#crossfade;
  }

  /** The engine's count of changes to this queue, as far as this mirror has followed them. */
  get version(): number {
    return this.#version;
  }

  /**
   * Adds tracks. Loaded tracks are queued as they are; queries (links, searches, a list from
   * `parseQueries`) are queued at once as loading items, which the engine loads in the
   * background, in order, into what they find: a search its first result, a playlist link its
   * tracks. `whenLoaded` tells when one has. With nothing playing, the first item of the queue
   * starts once it is ready.
   */
  async add(tracks: Playable | Playable[], options: AddOptions = {}): Promise<QueueAdded> {
    const added = await this.#request("queue.add", {
      guildId: this.guildId,
      tracks: (Array.isArray(tracks) ? tracks : [tracks]).map((track) =>
        selector(track, options.userData, options.startIndex),
      ),
      ...(options.index === undefined ? {} : { index: options.index }),
    });
    this._handleState(added.queue);
    return added;
  }

  /** Removes items by id; ids not in the queue are ignored. */
  remove(...ids: string[]): Promise<QueueState> {
    return this.#edit(this.#request("queue.remove", { guildId: this.guildId, ids }));
  }

  /** Moves an item to `index`, counted without the item. */
  move(id: string, index: number): Promise<QueueState> {
    return this.#edit(this.#request("queue.move", { guildId: this.guildId, id, index }));
  }

  clear(): Promise<QueueState> {
    return this.#edit(this.#request("queue.clear", { guildId: this.guildId }));
  }

  shuffle(): Promise<QueueState> {
    return this.#edit(this.#request("queue.shuffle", { guildId: this.guildId }));
  }

  setRepeat(repeat: Repeat): Promise<QueueState> {
    return this.#edit(this.#request("queue.configure", { guildId: this.guildId, repeat }));
  }

  /**
   * Overlaps the end of each track with the start of the next for `ms` (up to 12 000), one
   * fading out as the other fades in; 0 plays them back to back. At most half of a track fades,
   * and streams never do.
   */
  setCrossfade(ms: number): Promise<QueueState> {
    return this.#edit(
      this.#request("queue.configure", { guildId: this.guildId, crossfade: whole(ms) }),
    );
  }

  /** How many items are still loading. */
  get loading(): number {
    return this.#items.reduce((count, item) => count + (item.loading ? 1 : 0), 0);
  }

  /**
   * Resolves once the loading item `id` has loaded, with what took its place; at once for an item
   * that is not loading. Rejects, once the item has left `items`, with the error its query
   * failed with, or with a `not_found` error if it was removed (or was never queued) before it
   * loaded.
   */
  async whenLoaded(id: string): Promise<LoadedItems> {
    // an item added while the queue syncs shows in `items` once that is done
    if (this.#syncing !== null) {
      await this.#syncing.catch(() => undefined);
    }
    const known = this.#outcomes.get(id);
    if (known !== undefined) {
      if ("error" in known) {
        throw known.error;
      }
      return known.loaded;
    }
    const item = this.#items.find((candidate) => candidate.id === id);
    if (item === undefined) {
      throw notQueued(id);
    }
    if (!item.loading) {
      return { items: [item], playlist: null };
    }
    return new Promise((resolve, reject) => {
      const waiting = this.#waiting.get(id) ?? [];
      waiting.push((outcome) => ("loaded" in outcome ? resolve(outcome.loaded) : reject(outcome.error)));
      this.#waiting.set(id, waiting);
    });
  }

  /**
   * Fetches the whole queue again. The queue does this by itself when it notices missed
   * changes, so this is rarely needed.
   */
  sync(): Promise<void> {
    this.#syncing ??= this.#fetch().finally(() => {
      this.#syncing = null;
    });
    return this.#syncing;
  }

  /** @internal A `queueChanged` event. */
  _handleChange(version: number, change: QueueChange): void {
    if (this.#buffered !== null) {
      this.#buffered.push({ version, change });
    } else if (!this.#apply(version, change)) {
      this.#syncInBackground();
    }
  }

  /**
   * @internal A `trackError`: one for a loading item's stand-in (`sourceName` `query`) means its
   * query failed to load. Its waits end once its removal follows, so that a failed item has
   * left `items` by then.
   */
  _handleTrackError(itemId: string, track: Track, error: EntroximerError): void {
    if (track.info.sourceName === "query") {
      this.#failures.set(itemId, error);
    }
  }

  /**
   * @internal The queue's state in a response. Every change before the response has been
   * applied by now, so a newer version means some were missed.
   */
  _handleState(state: QueueState): void {
    // only clients change it, so the latest state has it right
    this.#crossfade = state.crossfade;
    if (this.#buffered === null && state.version > this.#version) {
      this.#syncInBackground();
    }
  }

  async #edit(request: Promise<QueueState>): Promise<QueueState> {
    const state = await request;
    this._handleState(state);
    return state;
  }

  /** Applies a change if it is the next one; returns `false` if some were missed. */
  #apply(version: number, change: QueueChange): boolean {
    if (version <= this.#version) {
      return true; // already part of a fetched queue
    }
    if (version !== this.#version + 1) {
      return false;
    }
    const items = this.#items;
    const without = (id: string) => items.filter((item) => item.id !== id);
    switch (change.type) {
      case "added":
        this.#items = [...items.slice(0, change.index), ...change.items, ...items.slice(change.index)];
        break;
      case "removed": {
        const removed = new Set(change.ids);
        this.#items = items.filter((item) => !removed.has(item.id));
        for (const id of change.ids) {
          this.#gone(id);
        }
        break;
      }
      case "moved": {
        const item = items.find((candidate) => candidate.id === change.id);
        const rest = without(change.id);
        this.#items =
          item === undefined
            ? rest
            : [...rest.slice(0, change.index), item, ...rest.slice(change.index)];
        break;
      }
      case "cleared":
        this.#items = [];
        for (const id of [...this.#waiting.keys(), ...this.#failures.keys()]) {
          this.#gone(id);
        }
        break;
      case "shuffled": {
        const byId = new Map(items.map((item) => [item.id, item]));
        this.#items = change.order.flatMap((id) => byId.get(id) ?? []);
        break;
      }
      case "updated":
        this.#items = items.map((item) => (item.id === change.item.id ? change.item : item));
        break;
      case "loaded": {
        const at = items.findIndex((item) => item.id === change.id);
        if (at !== -1) {
          this.#items = [...items.slice(0, at), ...change.items, ...items.slice(at + 1)];
        }
        this.#settle(change.id, { loaded: { items: change.items, playlist: change.playlist } });
        break;
      }
      case "advanced":
        this.#items = without(change.id);
        break;
      case "repeat":
        this.#repeat = change.repeat;
        break;
    }
    this.#version = version;
    this.emit("change", change);
    return true;
  }

  async #fetch(): Promise<void> {
    this.#buffered = [];
    try {
      for (let attempt = 1; ; attempt++) {
        const fetched = await this.#fetchPages();
        if (fetched !== null) {
          this.#items = fetched.items;
          this.#repeat = fetched.repeat;
          this.#version = fetched.version;
          // what changed meanwhile; if that misses changes again, fetch again
          const buffered = this.#buffered;
          this.#buffered = [];
          if (buffered.every(({ version, change }) => this.#apply(version, change))) {
            break;
          }
        }
        if (attempt === SYNC_ATTEMPTS) {
          throw new EntroximerError("internal", "the queue kept changing while it was fetched");
        }
      }
    } finally {
      this.#buffered = null;
    }
    // loads that ended among the changes missed
    for (const id of [...this.#waiting.keys(), ...this.#failures.keys()]) {
      const item = this.#items.find((candidate) => candidate.id === id);
      if (item === undefined) {
        this.#gone(id);
      } else if (!item.loading) {
        this.#settle(id, { loaded: { items: [item], playlist: null } });
      }
    }
    this.emit("sync");
  }

  /** Item `id` left the queue: its waits end with why it failed, if it did. */
  #gone(id: string): void {
    const error = this.#failures.get(id) ?? notQueued(id);
    this.#failures.delete(id);
    this.#settle(id, { error });
  }

  /** Ends the waits for item `id`, and keeps the outcome for later ones. */
  #settle(id: string, outcome: Outcome): void {
    // only the first outcome counts
    if (this.#outcomes.has(id)) {
      return;
    }
    this.#outcomes.set(id, outcome);
    for (const oldest of this.#outcomes.keys()) {
      if (this.#outcomes.size <= OUTCOMES_KEPT) {
        break;
      }
      this.#outcomes.delete(oldest);
    }
    for (const done of this.#waiting.get(id) ?? []) {
      done(outcome);
    }
    this.#waiting.delete(id);
  }

  /** The whole queue, or `null` if it changed between pages. */
  async #fetchPages(): Promise<{ items: QueueItem[]; repeat: Repeat; version: number } | null> {
    const items: QueueItem[] = [];
    let state: QueueState | null = null;
    do {
      let page;
      try {
        page = await this.#request("queue.get", {
          guildId: this.guildId,
          offset: items.length,
          limit: PAGE,
        });
      } catch (error) {
        // the engine creates its player on the first change; until then the queue is empty
        if (state === null && error instanceof EntroximerError && error.code === "not_found") {
          return { items: [], repeat: "off", version: 0 };
        }
        throw error;
      }
      if (state !== null && page.queue.version !== state.version) {
        return null;
      }
      state = page.queue;
      items.push(...page.items);
      if (page.items.length === 0) {
        break;
      }
    } while (items.length < state.length);
    return { items, repeat: state.repeat, version: state.version };
  }

  #syncInBackground(): void {
    this.sync().catch(this.#report);
  }
}

function notQueued(id: string): EntroximerError {
  return new EntroximerError("not_found", `the item \`${id}\` left the queue before it loaded`);
}
