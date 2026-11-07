import type { Requester } from "./connection.js";
import { Emitter } from "./emitter.js";
import { EntroximerError } from "./errors.js";
import type { ClipInfo } from "./generated/ClipInfo.js";
import type { ClipMode } from "./generated/ClipMode.js";
import type { Duck } from "./generated/Duck.js";
import type { Event } from "./generated/Event.js";
import type { GraphSpec } from "./generated/GraphSpec.js";
import type { LayerInfo } from "./generated/LayerInfo.js";
import type { LayerUpdate } from "./generated/LayerUpdate.js";
import type { Track } from "./generated/Track.js";
import type { TrackEndReason } from "./generated/TrackEndReason.js";
import type { JsonValue } from "./generated/serde_json/JsonValue.js";
import { selector } from "./tracks.js";
import type { Playable } from "./tracks.js";
import { whole } from "./units.js";

/** Clip events name the clip; every clip that `play` resolved ends once with `clipEnd`. */
export type LayerEvents = {
  /** A clip started; one that waited its turn and was stopped never does. */
  clipStart: [track: Track, clipId: string];
  clipEnd: [track: Track, reason: TrackEndReason, clipId: string];
  /** A clip failed; `clipEnd` with reason `failed` follows. */
  clipError: [track: Track, error: EntroximerError, clipId: string];
  /** A clip waited too long for its media; it ends as `failed`, so the ones after it play. */
  clipStuck: [track: Track, waitingMs: number, clipId: string];
};

export interface ClipOptions {
  /** The clip's id in its events; one is made up if absent. */
  clipId?: string;
  /** Percent of the layer's volume, up to 1000; 100 if absent. */
  volume?: number;
  /**
   * Where in the track to start, in milliseconds, in place of the link's start time (see
   * `parseLinkTimes`) or the track's own; `null` (or 0) plays from the beginning.
   */
  startAt?: number | null;
  /** Where in the track to stop, in milliseconds, in place of the link's or the track's; `null` plays to the end. */
  endAt?: number | null;
  /** Milliseconds to fade in, up to 10 000. */
  fadeIn?: number;
  /** Milliseconds to fade out when stopped or replaced, up to 10 000. */
  fadeOut?: number;
  /** Play again from the start whenever it ends, until stopped; not on `queue` layers. */
  loop?: boolean;
  /** Data echoed in the clip's track, e.g. who played it. */
  userData?: JsonValue;
}

/** Layer settings `configure` takes; absent ones are left alone. */
export type LayerChanges = Omit<LayerUpdate, "guildId" | "layer">;

/** What a layer needs from its player. */
export interface LayerHost {
  request: Requester;
  /** Throws if the player was destroyed. */
  ensureUsable(): void;
}

/**
 * One of a player's layers besides the music: sounds outside the queue (clips) with their own
 * effects, volume and pause, mixed over the music. The engine makes it with the first change or
 * clip; this object mirrors it from responses and events.
 */
export class Layer extends Emitter<LayerEvents> {
  /** What a new clip does while others play: wait its turn, play over them, or end them. */
  mode: ClipMode = "queue";
  /** With `mix`: how many clips play at once. */
  maxClips = 8;
  /** Percent; 100 leaves the audio unchanged. */
  volume = 100;
  /** Whether the audio is hard clipped after the volume; see `setClipping`. */
  clipping = false;
  paused = false;
  graph: GraphSpec | null = null;
  /** The layers this one lowers while its clips are heard. */
  duck: Duck | null = null;

  #clips: readonly ClipInfo[] = [];
  /** Clips `play` sent, until it resolves: whether they ended meanwhile. */
  readonly #sent = new Map<string, boolean>();
  readonly #host: LayerHost;

  /** @internal Created by its player. */
  constructor(
    readonly guildId: string,
    readonly name: string,
    host: LayerHost,
  ) {
    super();
    this.#host = host;
  }

  /** The clips playing, then those waiting their turn; replaced, not changed, on every change. */
  get clips(): readonly ClipInfo[] {
    return this.#clips;
  }

  /** Changes the layer, creating it if needed; a refused change changes nothing. */
  async configure(changes: LayerChanges): Promise<LayerInfo> {
    this.#host.ensureUsable();
    const info = await this.#host.request("layer.update", {
      ...changes,
      guildId: this.guildId,
      layer: this.name,
    });
    this._apply(info);
    return info;
  }

  /** Percent: 100 leaves the audio unchanged, up to 1000; with `clipping`, also sets that. */
  setVolume(percent: number, options: { clipping?: boolean } = {}): Promise<LayerInfo> {
    return this.configure({ volume: whole(percent), ...options });
  }

  /** Hard clips this layer's audio after its volume, as the player's `setClipping` does the music's. */
  setClipping(clipping: boolean): Promise<LayerInfo> {
    return this.configure({ clipping });
  }

  setGraph(graph: GraphSpec | null): Promise<LayerInfo> {
    return this.configure({ graph });
  }

  setPreset(name: string, params?: Record<string, number>): Promise<LayerInfo> {
    return this.configure({ preset: { name, ...(params === undefined ? {} : { params }) } });
  }

  pause(): Promise<LayerInfo> {
    return this.configure({ paused: true });
  }

  resume(): Promise<LayerInfo> {
    return this.configure({ paused: false });
  }

  /** Lowers other layers (`main` is the music) while this layer's clips are heard; `null` stops. */
  setDuck(duck: Duck | null): Promise<LayerInfo> {
    if (duck === null) {
      return this.configure({ duck });
    }
    const { layers, volume, attack, release, hold } = duck;
    return this.configure({
      duck: {
        layers,
        volume: whole(volume),
        ...(attack === undefined ? {} : { attack: whole(attack) }),
        ...(release === undefined ? {} : { release: whole(release) }),
        ...(hold === undefined ? {} : { hold: whole(hold) }),
      },
    });
  }

  /**
   * Plays a sound on this layer, outside the queue. Its `clipStart` comes before this resolves
   * if it starts right away; clips played on the same layer join it in the order they were
   * played.
   */
  async play(track: Playable, options: ClipOptions = {}): Promise<ClipInfo> {
    this.#host.ensureUsable();
    // an id known before sending, so that events coming before the response can be told apart
    const clipId = options.clipId ?? crypto.randomUUID();
    this.#sent.set(clipId, false);
    try {
      const clip = await this.#host.request("clip.play", {
        guildId: this.guildId,
        layer: this.name,
        clipId,
        track: selector(track, { userData: options.userData, startTime: options.startAt, endTime: options.endAt }),
        ...(options.volume === undefined ? {} : { volume: whole(options.volume) }),
        ...(options.fadeIn === undefined ? {} : { fadeIn: whole(options.fadeIn) }),
        ...(options.fadeOut === undefined ? {} : { fadeOut: whole(options.fadeOut) }),
        ...(options.loop === undefined ? {} : { loop: options.loop }),
      });
      // it may have ended already, before the response came
      if (this.#sent.get(clipId) !== true) {
        this.#upsert(clip);
      }
      return clip;
    } finally {
      this.#sent.delete(clipId);
    }
  }

  /**
   * Stops a clip, or with no `clipId` every clip of the layer, waiting ones too; they fade out
   * over `fadeOut` milliseconds, or as each was played with.
   */
  async stop(clipId?: string, fadeOut?: number): Promise<LayerInfo> {
    this.#host.ensureUsable();
    const info = await this.#host.request("clip.stop", {
      guildId: this.guildId,
      layer: this.name,
      ...(clipId === undefined ? {} : { clipId }),
      ...(fadeOut === undefined ? {} : { fadeOut: whole(fadeOut) }),
    });
    this._apply(info);
    return info;
  }

  /** Removes the layer from the engine, ending its clips; it is made again by the next change. */
  async remove(): Promise<void> {
    this.#host.ensureUsable();
    try {
      await this.#host.request("layer.remove", { guildId: this.guildId, layer: this.name });
    } catch (error) {
      if (!(error instanceof EntroximerError && error.code === "not_found")) {
        throw error;
      }
    }
    this._apply(null);
  }

  /** @internal The layer as the engine reports it; `null` if it has no such layer. */
  _apply(info: LayerInfo | null): void {
    this.mode = info?.mode ?? "queue";
    this.maxClips = info?.maxClips ?? 8;
    this.volume = info?.volume ?? 100;
    this.clipping = info?.clipping ?? false;
    this.paused = info?.paused ?? false;
    this.graph = info?.graph ?? null;
    this.duck = info?.duck ?? null;
    this.#clips = info?.clips ?? [];
  }

  /** @internal A clip event of this layer. */
  _handleEvent(event: Event): void {
    switch (event.event) {
      case "clipStart":
        this.#upsert({
          clipId: event.clipId,
          layer: this.name,
          track: event.track,
          status: "playing",
          position: this.#clips.find((clip) => clip.clipId === event.clipId)?.position ?? 0,
        });
        this.emit("clipStart", event.track, event.clipId);
        break;
      case "clipEnd":
        if (this.#sent.has(event.clipId)) {
          this.#sent.set(event.clipId, true);
        }
        this.#clips = this.#clips.filter((clip) => clip.clipId !== event.clipId);
        this.emit("clipEnd", event.track, event.reason, event.clipId);
        break;
      case "clipError":
        this.emit("clipError", event.track, EntroximerError.fromApi(event.error), event.clipId);
        break;
      case "clipStuck":
        this.emit("clipStuck", event.track, event.waiting, event.clipId);
        break;
    }
  }

  /** Puts a clip in the mirror, or updates it there. */
  #upsert(clip: ClipInfo): void {
    const index = this.#clips.findIndex((candidate) => candidate.clipId === clip.clipId);
    this.#clips =
      index === -1
        ? [...this.#clips, clip]
        : this.#clips.map((candidate, i) => (i === index ? clip : candidate));
  }
}
