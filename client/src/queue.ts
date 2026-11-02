import type { Requester } from "./connection";
import { Emitter } from "./emitter";
import { EntroximerError } from "./errors";
import type { QueueAdded } from "./generated/QueueAdded";
import type { QueueChange } from "./generated/QueueChange";
import type { QueueItem } from "./generated/QueueItem";
import type { QueueState } from "./generated/QueueState";
import type { Repeat } from "./generated/Repeat";
import type { JsonValue } from "./generated/serde_json/JsonValue";
import { selector } from "./tracks";
import type { Playable } from "./tracks";

/** Items one `queue.get` fetches while syncing. */
const PAGE = 1000;
/** Syncing starts over if the queue changes between its pages, at most this often. */
const SYNC_ATTEMPTS = 5;

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
}

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
   * Adds tracks; queries are loaded by the engine, a search adding its first result and a
   * playlist link all its tracks. With nothing playing, the first item of the queue starts.
   */
  async add(tracks: Playable | Playable[], options: AddOptions = {}): Promise<QueueAdded> {
    const added = await this.#request("queue.add", {
      guildId: this.guildId,
      tracks: (Array.isArray(tracks) ? tracks : [tracks]).map((track) =>
        selector(track, options.userData),
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
      this.#request("queue.configure", { guildId: this.guildId, crossfade: ms }),
    );
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
        break;
      case "shuffled": {
        const byId = new Map(items.map((item) => [item.id, item]));
        this.#items = change.order.flatMap((id) => byId.get(id) ?? []);
        break;
      }
      case "updated":
        this.#items = items.map((item) => (item.id === change.item.id ? change.item : item));
        break;
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
    this.emit("sync");
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
