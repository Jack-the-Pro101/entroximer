import type { Requester } from "./connection";
import { Emitter } from "./emitter";
import { EntroximerError, toEntroximerError } from "./errors";
import type { Event } from "./generated/Event";
import type { GraphSpec } from "./generated/GraphSpec";
import type { PlayerInfo } from "./generated/PlayerInfo";
import type { PlayerState } from "./generated/PlayerState";
import type { PlayerUpdate } from "./generated/PlayerUpdate";
import type { Track } from "./generated/Track";
import type { TrackEndReason } from "./generated/TrackEndReason";
import type { VoiceState } from "./generated/VoiceState";
import type { JsonValue } from "./generated/serde_json/JsonValue";
import { Layer } from "./layer";
import { Queue } from "./queue";
import { selector } from "./tracks";
import type { Playable } from "./tracks";
import { whole } from "./units";

export interface PlayOptions {
  /** Data echoed back in this track's events, e.g. who requested it. */
  userData?: JsonValue;
  /** Where to start, in milliseconds. */
  startAt?: number;
  /** Where to stop, in milliseconds. */
  endAt?: number;
  /** Only start the track if nothing is playing. */
  onlyIfIdle?: boolean;
}

export interface VoiceClosedInfo {
  /** Discord's voice close code, if it sent one. */
  code?: number;
  reason: string;
  byRemote: boolean;
}

/** Track events name the track's queue item; tracks played with `play` have one too. */
type PlayerEvents = {
  trackStart: [track: Track, itemId: string];
  trackEnd: [track: Track, reason: TrackEndReason, itemId: string];
  /**
   * A playing track failed, and `trackEnd` with reason `failed` follows; or a queued track
   * failed to load, and was removed from the queue, which goes on with the next one.
   */
  trackError: [track: Track, error: EntroximerError, itemId: string];
  trackStuck: [track: Track, waitingMs: number, itemId: string];
  /** Playback stopped because nothing is left to play. */
  queueEnd: [];
  /** A clip event of one of the player's layers, which emits it too (see `Layer`). */
  clipStart: [layer: string, track: Track, clipId: string];
  clipEnd: [layer: string, track: Track, reason: TrackEndReason, clipId: string];
  clipError: [layer: string, track: Track, error: EntroximerError, clipId: string];
  clipStuck: [layer: string, track: Track, waitingMs: number, clipId: string];
  update: [state: PlayerState];
  voiceClosed: [info: VoiceClosedInfo];
  /** `setTextChannel` moved the player's text channel. */
  textChannelChange: [channelId: string | null, previous: string | null];
  /** A background request (e.g. passing on voice details) failed. */
  error: [error: EntroximerError];
  /** The player is gone; its object can no longer be used. */
  destroyed: [];
};

/** What a player needs from its manager. */
export interface PlayerHost {
  request: Requester;
  destroy(guildId: string): Promise<void>;
  joinVoice(guildId: string, channelId: string | null, options: JoinOptions): void;
}

export interface JoinOptions {
  /** Default `true`: the bot does not hear the channel, which saves bandwidth. */
  selfDeaf?: boolean;
  selfMute?: boolean;
}

/**
 * One guild's playback. Created by `Entroximer.player(guildId)`; the engine creates its side on
 * the first change.
 */
export class Player extends Emitter<PlayerEvents> {
  /** The playing track. */
  track: Track | null = null;
  /** The playing track's queue item. */
  itemId: string | null = null;
  /** The tracks that play after `track`. */
  readonly queue: Queue;
  paused = false;
  /** Percent; 100 leaves the audio unchanged. */
  volume = 100;
  graph: GraphSpec | null = null;
  /** The engine's latest report; see also `position`. */
  state: PlayerState | null = null;
  /** The voice channel the bot is in, as Discord reports it. */
  channelId: string | null = null;

  readonly #host: PlayerHost;
  /** When `state` arrived, by this machine's clock, which may differ from the engine's. */
  #stateReceivedAt = 0;
  #destroyed = false;
  #destroying: Promise<void> | null = null;
  #voiceSession: string | null = null;
  #voiceServer: { token: string; endpoint: string } | null = null;
  #sentVoice: VoiceState | null = null;
  /** The other gateway session last reported for joining voice in this guild. */
  #conflictingSession: string | null = null;
  #textChannelId: string | null = null;
  readonly #layers = new Map<string, Layer>();

  constructor(
    readonly guildId: string,
    host: PlayerHost,
  ) {
    super();
    this.#host = host;
    this.queue = new Queue(guildId, host.request, (error) => this.#report(error));
  }

  get destroyed(): boolean {
    return this.#destroyed;
  }

  /**
   * One of the player's layers besides the music (`main`, which this object itself controls):
   * sounds outside the queue, such as announcements or sound effects, with their own effects,
   * volume and pause. The engine makes the layer with its first change or clip.
   */
  layer(name: string): Layer {
    let layer = this.#layers.get(name);
    if (layer === undefined) {
      layer = new Layer(this.guildId, name, {
        request: this.#host.request,
        ensureUsable: () => this.#ensureUsable(),
      });
      this.#layers.set(name, layer);
    }
    return layer;
  }

  /** The layers the engine reported or this client used, by name. */
  get layers(): ReadonlyMap<string, Layer> {
    return this.#layers;
  }

  /**
   * The text channel the bot posts about this player in, e.g. what starts playing; `null` if
   * none. The client only keeps it, for as long as the player lives (and readable after), and
   * the engine never sees it.
   */
  get textChannelId(): string | null {
    return this.#textChannelId;
  }

  /** Moves the player's text channel, or removes it with `null`; see `textChannelId`. */
  setTextChannel(channelId: string | null): void {
    this.#ensureUsable();
    const previous = this.#textChannelId;
    if (channelId !== previous) {
      this.#textChannelId = channelId;
      this.emit("textChannelChange", channelId, previous);
    }
  }

  /**
   * The current position in milliseconds, extrapolated from the engine's last report (at the
   * graph's playback rate, if it sets one).
   */
  get position(): number {
    const state = this.state;
    if (!state) {
      return 0;
    }
    const flowing = this.track !== null && !this.paused && state.connected && !state.buffering;
    const rate = this.graph?.rate ?? 1;
    const elapsed = flowing ? Math.max(0, Date.now() - this.#stateReceivedAt) * rate : 0;
    const length = this.track?.info.length;
    const position = state.position + elapsed;
    return length == null ? position : Math.min(position, length);
  }

  /** Joins a voice channel, or moves to another one. */
  connect(channelId: string, options: JoinOptions = {}): void {
    this.#ensureUsable();
    this.#host.joinVoice(this.guildId, channelId, options);
  }

  /**
   * Plays `track` now, in place of the current one. The queue stays as it is and goes on once
   * the track ends.
   */
  play(track: Playable, options: PlayOptions = {}): Promise<PlayerInfo> {
    return this.update({
      track: selector(track, options.userData),
      ...(options.startAt === undefined ? {} : { position: whole(options.startAt) }),
      ...(options.endAt === undefined ? {} : { endTime: whole(options.endAt) }),
      ...(options.onlyIfIdle === undefined ? {} : { onlyIfIdle: options.onlyIfIdle }),
    });
  }

  /**
   * Ends the current track and plays the queue's first item, or the item `toItem` after
   * removing the ones before it; stops if the queue is empty. Instant when the engine has
   * loaded that item already; otherwise the current track plays on until the item is ready.
   */
  async skip(toItem?: string): Promise<PlayerInfo> {
    this.#ensureUsable();
    const info = await this.#host.request("queue.skip", {
      guildId: this.guildId,
      ...(toItem === undefined ? {} : { toItem }),
    });
    this.#apply(info);
    return info;
  }

  seek(positionMs: number): Promise<PlayerInfo> {
    return this.update({ position: whole(positionMs) });
  }

  pause(): Promise<PlayerInfo> {
    return this.update({ paused: true });
  }

  resume(): Promise<PlayerInfo> {
    return this.update({ paused: false });
  }

  /**
   * Stops the current track. The queue stays, waiting until something plays: with `play`,
   * `skip` or by adding to the queue.
   */
  stop(): Promise<PlayerInfo> {
    return this.update({ track: null });
  }

  /** Percent: 100 leaves the audio unchanged, up to 1000. */
  setVolume(percent: number): Promise<PlayerInfo> {
    return this.update({ volume: whole(percent) });
  }

  /** Where the current track stops, in milliseconds; `null` plays it to the end. */
  setEndTime(positionMs: number | null): Promise<PlayerInfo> {
    return this.update({ endTime: positionMs === null ? null : whole(positionMs) });
  }

  /**
   * The effect graph, e.g. from `chain()` or `parseChain()`, or a preset's extended with
   * `append()`; `null` removes it.
   */
  setGraph(graph: GraphSpec | null): Promise<PlayerInfo> {
    return this.update({ graph });
  }

  /**
   * Replaces the effects with a preset's (see `NodeInfo.presets`), e.g. `setPreset("nightcore")`
   * or `setPreset("bass_boost", { gain_db: 9 })`. Afterwards `graph` holds the nodes it built,
   * to change with `withParams()` or extend with `append()`.
   */
  setPreset(name: string, params?: Record<string, number>): Promise<PlayerInfo> {
    return this.update({ preset: params === undefined ? { name } : { name, params } });
  }

  /** Applies several changes at once. */
  async update(changes: Omit<PlayerUpdate, "guildId">): Promise<PlayerInfo> {
    this.#ensureUsable();
    const info = await this.#host.request("player.update", { ...changes, guildId: this.guildId });
    this.#apply(info);
    return info;
  }

  /** Fetches the player's state and queue again, e.g. after events were missed. */
  async refresh(): Promise<void> {
    this.#ensureUsable();
    try {
      this.#apply(await this.#host.request("player.get", { guildId: this.guildId }));
    } catch (error) {
      // the engine creates its player on the first change
      if (!(error instanceof EntroximerError && error.code === "not_found")) {
        throw error;
      }
    }
    await this.queue.sync();
  }

  /** Stops playback, leaves the voice channel and removes the player. */
  destroy(): Promise<void> {
    // leaving the channel makes Discord report it, which would destroy the player again
    this.#destroying ??= (async () => {
      this.#host.joinVoice(this.guildId, null, {});
      await this.#host.destroy(this.guildId);
    })();
    return this.#destroying;
  }

  /** @internal Called by the manager for this guild's events. */
  _handleEvent(event: Event): void {
    switch (event.event) {
      case "trackStart":
        this.track = event.track;
        this.itemId = event.itemId;
        this.emit("trackStart", event.track, event.itemId);
        break;
      case "trackEnd":
        if (this.itemId === event.itemId) {
          this.track = null;
          this.itemId = null;
        }
        this.emit("trackEnd", event.track, event.reason, event.itemId);
        break;
      case "trackError":
        this.emit("trackError", event.track, EntroximerError.fromApi(event.error), event.itemId);
        break;
      case "trackStuck":
        this.emit("trackStuck", event.track, event.waiting, event.itemId);
        break;
      case "queueChanged":
        this.queue._handleChange(event.version, event.change);
        break;
      case "queueEnd":
        this.emit("queueEnd");
        break;
      case "playerUpdate":
        this.#setState(event.state);
        this.emit("update", event.state);
        break;
      case "clipStart":
        this.layer(event.layer)._handleEvent(event);
        this.emit("clipStart", event.layer, event.track, event.clipId);
        break;
      case "clipEnd":
        this.layer(event.layer)._handleEvent(event);
        this.emit("clipEnd", event.layer, event.track, event.reason, event.clipId);
        break;
      case "clipError":
        this.layer(event.layer)._handleEvent(event);
        this.emit(
          "clipError",
          event.layer,
          event.track,
          EntroximerError.fromApi(event.error),
          event.clipId,
        );
        break;
      case "clipStuck":
        this.layer(event.layer)._handleEvent(event);
        this.emit("clipStuck", event.layer, event.track, event.waiting, event.clipId);
        break;
      case "voiceClosed":
        // the engine forgets details it gave up on; send them again if Discord does
        this.#sentVoice = null;
        this.emit("voiceClosed", {
          reason: event.reason,
          byRemote: event.byRemote,
          ...(event.code === undefined ? {} : { code: event.code }),
        });
        break;
    }
  }

  /**
   * @internal Discord's voice state for the bot in this guild; `ours` is false if it belongs to
   * another gateway session of the bot.
   */
  _handleVoiceState(sessionId: string, channelId: string | null, ours = true): void {
    if (!ours) {
      // passing it on would make both sessions take the voice connection from each other
      if (channelId !== null && this.#conflictingSession !== sessionId) {
        this.#conflictingSession = sessionId;
        this.#report(
          new EntroximerError(
            "voice_conflict",
            `another session of this bot joined voice in guild ${this.guildId}, which ends this ` +
              "player's voice connection; is the bot running twice?",
          ),
        );
      }
      return;
    }
    this.#conflictingSession = null;
    this.channelId = channelId;
    if (channelId === null) {
      // the bot left or was disconnected from the voice channel
      this.destroy().catch((error: unknown) => this.#report(error));
      return;
    }
    this.#voiceSession = sessionId;
    this.#sendVoice();
  }

  /** @internal Discord's voice server for this guild. `endpoint` is null while it allocates one. */
  _handleVoiceServer(token: string, endpoint: string | null): void {
    this.#voiceServer = endpoint === null ? null : { token, endpoint };
    this.#sendVoice();
  }

  /** @internal The player is gone, e.g. its session expired while disconnected. */
  _markDestroyed(): void {
    if (!this.#destroyed) {
      this.#destroyed = true;
      this.emit("destroyed");
    }
  }

  #apply(info: PlayerInfo): void {
    this.track = info.track;
    this.itemId = info.itemId;
    this.queue._handleState(info.queue);
    this.paused = info.paused;
    this.volume = info.volume;
    this.graph = info.graph;
    this.#setState(info.state);
    for (const layer of info.layers) {
      this.layer(layer.name)._apply(layer);
    }
    for (const [name, layer] of this.#layers) {
      if (!info.layers.some((reported) => reported.name === name)) {
        layer._apply(null);
      }
    }
  }

  #setState(state: PlayerState): void {
    this.state = state;
    this.#stateReceivedAt = Date.now();
  }

  /** Passes complete, changed voice details on to the engine. */
  #sendVoice(): void {
    if (this.#voiceSession === null || this.#voiceServer === null || this.channelId === null) {
      return;
    }
    const voice: VoiceState = {
      token: this.#voiceServer.token,
      endpoint: this.#voiceServer.endpoint,
      sessionId: this.#voiceSession,
      channelId: this.channelId,
    };
    const sent = this.#sentVoice;
    if (
      sent?.token === voice.token &&
      sent.endpoint === voice.endpoint &&
      sent.sessionId === voice.sessionId &&
      sent.channelId === voice.channelId
    ) {
      return;
    }
    this.#sentVoice = voice;
    this.update({ voice }).catch((error: unknown) => {
      // sent again when Discord repeats them, unless newer ones went out meanwhile
      if (this.#sentVoice === voice) {
        this.#sentVoice = null;
      }
      this.#report(error);
    });
  }

  #report(error: unknown): void {
    this.emit("error", toEntroximerError(error));
  }

  #ensureUsable(): void {
    if (this.#destroyed) {
      throw new EntroximerError("not_found", `the player for guild ${this.guildId} was destroyed`);
    }
  }
}
