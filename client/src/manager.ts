import { Connection, PROTOCOL_VERSION } from "./connection";
import type { ConnectionOptions, DisconnectInfo, ReadyInfo } from "./connection";
import { Emitter } from "./emitter";
import { EntroximerError, toEntroximerError } from "./errors";
import type { Event } from "./generated/Event";
import type { GraphSpec } from "./generated/GraphSpec";
import type { LoadResult } from "./generated/LoadResult";
import type { NodeInfo } from "./generated/NodeInfo";
import type { SearchResult } from "./generated/SearchResult";
import type { SessionState } from "./generated/SessionState";
import type { Stats } from "./generated/Stats";
import type { Track } from "./generated/Track";
import { Player } from "./player";
import type { JoinOptions, PlayerHost } from "./player";

/** The voice state update a bot sends on Discord's gateway to join or leave a voice channel. */
export interface GatewayVoiceStateUpdate {
  op: 4;
  d: {
    guild_id: string;
    channel_id: string | null;
    self_mute: boolean;
    self_deaf: boolean;
  };
}

/** A gateway dispatch, as Discord libraries pass them on (discord.js: the `raw` event). */
export interface GatewayDispatch {
  t?: string | null;
  d?: unknown;
}

export interface SearchOptions {
  /** A search prefix from `info().providers`, e.g. `scsearch`; the engine's default search if absent. */
  prefix?: string;
  /** Up to 50; 10 if absent. */
  limit?: number;
}

export interface EntroximerOptions extends ConnectionOptions {
  /** Sends a payload on the gateway shard of `guildId`; with discord.js: `guild.shard.send(payload)`. */
  sendToGateway: (guildId: string, payload: GatewayVoiceStateUpdate) => void;
  /** How long players wait, paused, for this client to reconnect before they are destroyed. */
  resumeTimeoutSecs?: number;
}

type ManagerEvents = {
  ready: [info: ReadyInfo];
  disconnect: [info: DisconnectInfo];
  /** Every event from the engine; players get their own as well. */
  event: [event: Event];
  stats: [stats: Stats];
  /** The engine's YouTube session changed, e.g. it expired and needs fresh cookies. */
  youtubeSession: [state: SessionState];
  /** The client reconnected too late: the engine had destroyed these players. */
  playersLost: [players: Player[]];
  /** A background request failed. */
  error: [error: EntroximerError];
};

/**
 * The client for one engine: loads tracks and manages a `Player` per guild.
 *
 * Feed it the bot's gateway events with `handleRawEvent`, so that players can connect to voice.
 */
export class Entroximer extends Emitter<ManagerEvents> {
  readonly connection: Connection;
  readonly #players = new Map<string, Player>();
  readonly #userId: string;
  readonly #sendToGateway: EntroximerOptions["sendToGateway"];
  readonly #resumeTimeoutSecs: number | undefined;
  readonly #host: PlayerHost;
  #connectedBefore = false;

  constructor(options: EntroximerOptions) {
    super();
    this.connection = new Connection(options);
    this.#userId = options.userId;
    this.#sendToGateway = options.sendToGateway;
    this.#resumeTimeoutSecs = options.resumeTimeoutSecs;
    this.#host = {
      request: (op, ...params) => this.connection.request(op, ...params),
      destroy: (guildId) => this.#destroyPlayer(guildId),
      joinVoice: (guildId, channelId, join) => this.#joinVoice(guildId, channelId, join),
    };
    this.connection.on("ready", (info) => this.#onReady(info));
    this.connection.on("disconnect", (info) => this.emit("disconnect", info));
    this.connection.on("event", (event) => this.#onEvent(event));
  }

  get players(): ReadonlyMap<string, Player> {
    return this.#players;
  }

  /** Connects, checks that the engine speaks this client's protocol, and returns its info. */
  async connect(): Promise<NodeInfo> {
    await this.connection.connect();
    const info = await this.info();
    if (info.protocol !== PROTOCOL_VERSION) {
      this.close();
      throw new EntroximerError(
        "protocol_mismatch",
        `the engine speaks protocol ${info.protocol}, this client ${PROTOCOL_VERSION}`,
      );
    }
    return info;
  }

  /** Disconnects for good. The engine destroys the players once the resume timeout passes. */
  close(): void {
    this.connection.close();
  }

  /**
   * Loads a link (a track or a playlist), a search (`ytsearch:...`, or plain text for the
   * engine's default search) or, if the engine allows it, a path on its host. Search results and
   * playlist tracks carry metadata only; the engine resolves them when they play.
   */
  load(query: string): Promise<LoadResult> {
    return this.connection.request("tracks.load", { query });
  }

  /** Searches one source; see `SearchOptions`. Results are cached by the engine for a while. */
  search(query: string, options: SearchOptions = {}): Promise<SearchResult> {
    return this.connection.request("tracks.search", { query, ...options });
  }

  /** Reads tracks back from their `encoded` form. */
  decode(encoded: string[]): Promise<Track[]> {
    return this.connection.request("tracks.decode", { encoded });
  }

  /**
   * The effect graph a preset builds (see `info().presets`), without applying it: to extend it
   * with `append()` before `player.setGraph()`. `player.setPreset()` applies one directly.
   */
  buildPreset(name: string, params?: Record<string, number>): Promise<GraphSpec> {
    return this.connection.request("presets.build", params === undefined ? { name } : { name, params });
  }

  info(): Promise<NodeInfo> {
    return this.connection.request("node.info");
  }

  stats(): Promise<Stats> {
    return this.connection.request("node.stats");
  }

  /** The guild's player, created on first use. */
  player(guildId: string): Player {
    let player = this.#players.get(guildId);
    if (!player) {
      player = new Player(guildId, this.#host);
      this.#players.set(guildId, player);
    }
    return player;
  }

  /**
   * Passes on the gateway dispatches players need (the bot's voice state and voice server
   * updates); others are ignored. With discord.js: `client.on(Events.Raw, (p) => manager.handleRawEvent(p))`.
   */
  handleRawEvent(dispatch: GatewayDispatch): void {
    if (dispatch.t === "VOICE_STATE_UPDATE") {
      const d = dispatch.d as {
        guild_id?: string;
        user_id: string;
        session_id: string;
        channel_id: string | null;
      };
      if (d.user_id === this.#userId && d.guild_id !== undefined) {
        this.#players.get(d.guild_id)?._handleVoiceState(d.session_id, d.channel_id);
      }
    } else if (dispatch.t === "VOICE_SERVER_UPDATE") {
      const d = dispatch.d as { guild_id: string; token: string; endpoint: string | null };
      this.#players.get(d.guild_id)?._handleVoiceServer(d.token, d.endpoint);
    }
  }

  #onReady(info: ReadyInfo): void {
    if (this.#connectedBefore && !info.resumed && this.#players.size > 0) {
      const lost = [...this.#players.values()];
      this.#players.clear();
      for (const player of lost) {
        player._markDestroyed();
      }
      this.emit("playersLost", lost);
    }
    this.#connectedBefore = true;
    if (info.resumed && info.missedEvents > 0) {
      // the players continued, but some of what happened meanwhile was not kept
      for (const player of this.#players.values()) {
        player.refresh().catch((error: unknown) => this.#report(error));
      }
    }
    if (this.#resumeTimeoutSecs !== undefined) {
      this.connection
        .request("session.configure", { resumeTimeoutSecs: this.#resumeTimeoutSecs })
        .catch((error: unknown) => this.#report(error));
    }
    this.emit("ready", info);
  }

  #onEvent(event: Event): void {
    if ("guildId" in event) {
      this.#players.get(event.guildId)?._handleEvent(event);
    } else if (event.event === "stats") {
      const { event: _, ...stats } = event;
      this.emit("stats", stats);
    } else if (event.event === "youtubeSession") {
      this.emit("youtubeSession", event.state);
    }
    this.emit("event", event);
  }

  async #destroyPlayer(guildId: string): Promise<void> {
    const player = this.#players.get(guildId);
    this.#players.delete(guildId);
    try {
      await this.connection.request("player.destroy", { guildId });
    } catch (error) {
      // the engine never had this player, e.g. it was only asked to join voice
      if (!(error instanceof EntroximerError && error.code === "not_found")) {
        throw error;
      }
    } finally {
      player?._markDestroyed();
    }
  }

  #joinVoice(guildId: string, channelId: string | null, join: JoinOptions): void {
    this.#sendToGateway(guildId, {
      op: 4,
      d: {
        guild_id: guildId,
        channel_id: channelId,
        self_mute: join.selfMute ?? false,
        self_deaf: join.selfDeaf ?? true,
      },
    });
  }

  #report(error: unknown): void {
    this.emit("error", toEntroximerError(error));
  }
}
