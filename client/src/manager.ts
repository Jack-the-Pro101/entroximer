import { Connection, PROTOCOL_VERSION } from "./connection";
import type { ConnectionOptions, DisconnectInfo, ReadyInfo } from "./connection";
import { Emitter } from "./emitter";
import { EntroximerError, fromResponse, toEntroximerError } from "./errors";
import type { Event } from "./generated/Event";
import type { GraphSpec } from "./generated/GraphSpec";
import type { LoadResult } from "./generated/LoadResult";
import type { NodeInfo } from "./generated/NodeInfo";
import type { SearchResult } from "./generated/SearchResult";
import type { SessionState } from "./generated/SessionState";
import type { Stats } from "./generated/Stats";
import type { Track } from "./generated/Track";
import type { Upload } from "./generated/Upload";
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

/** Where a track's artwork is. */
export interface ArtworkLocation {
  /** An absolute URL. */
  url: string;
  /**
   * Whether anyone can fetch it, e.g. Discord for an embed. Covers the engine extracted from
   * media files are on the engine and need its password instead: use `fetchArtwork`, and upload
   * the image where it is shown.
   */
  public: boolean;
}

export interface SearchOptions {
  /** A search prefix from `info().providers`, e.g. `scsearch`; the engine's default search if absent. */
  prefix?: string;
  /** Up to 50; 10 if absent. */
  limit?: number;
}

export interface UploadOptions {
  /** Its title; else the file's own, from its tags. */
  title?: string;
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
  /**
   * The client reconnected too late, or the engine restarted: these players are gone, and the bot
   * has left their voice channels.
   */
  playersLost: [players: Player[]];
  /** A background request failed, or a player's did (`guildId` names it). */
  error: [error: EntroximerError, guildId?: string];
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
  /** This process's gateway sessions, from `READY`; empty if none was passed on. */
  readonly #gatewaySessions = new Set<string>();
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

  /** Where `track`'s artwork is, if it has any. */
  artwork(track: Track): ArtworkLocation | null {
    const url = track.info.artworkUrl;
    if (url === null) {
      return null;
    }
    // the engine's own artwork is given as a path on it
    return url.startsWith("/") ? { url: this.connection.httpUrl(url), public: false } : { url, public: true };
  }

  /**
   * `track`'s artwork as an image, from the web or from the engine; `null` if it has none or it
   * cannot be fetched, since artwork is never worth failing over.
   */
  async fetchArtwork(track: Track, timeoutMs = 5_000): Promise<Blob | null> {
    const location = this.artwork(track);
    if (location === null) {
      return null;
    }
    const signal = AbortSignal.timeout(timeoutMs);
    try {
      const response = location.public
        ? await fetch(location.url, { signal })
        : await this.connection.fetch(track.info.artworkUrl!, { signal });
      return response.ok ? await response.blob() : null;
    } catch {
      return null;
    }
  }

  /**
   * Uploads an audio file to play, if the engine's operator allows uploads (`info().uploads`), for
   * sounds that have no URL: anything with one, such as a Discord attachment, plays with `load()`
   * as it is. The returned track plays like any other until the upload expires, which it does
   * once unused for `uploads.keep`; the engine also removes the uploads unused for longest when it
   * needs room for new ones, but never one that is playing.
   *
   * Fails with `too_large` above `uploads.maxBytes` (before sending anything), `no_audio` for a
   * file the engine cannot play, and `storage_full` while every upload the engine keeps is
   * playing.
   */
  async upload(data: Blob | ArrayBuffer | Uint8Array<ArrayBuffer>, options: UploadOptions = {}): Promise<Upload> {
    const { uploads } = await this.info();
    if (uploads === null) {
      throw new EntroximerError("not_found", "uploads are off on this engine");
    }
    const bytes = data instanceof Blob ? data.size : data.byteLength;
    if (bytes > uploads.maxBytes) {
      throw new EntroximerError(
        "too_large",
        `the file is ${bytes} bytes, more than the engine takes (${uploads.maxBytes})`,
      );
    }
    const query = options.title === undefined ? "" : `?${new URLSearchParams({ title: options.title })}`;
    const response = await this.#http(`/v1/uploads${query}`, { method: "POST", body: data });
    if (!response.ok) {
      throw await fromResponse(response);
    }
    return (await response.json()) as Upload;
  }

  /**
   * Deletes an upload before it expires. If it is playing, it plays on, but cannot be played
   * again.
   */
  async deleteUpload(id: string): Promise<void> {
    const response = await this.#http(`/v1/uploads/${encodeURIComponent(id)}`, { method: "DELETE" });
    if (!response.ok) {
      throw await fromResponse(response);
    }
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
      player.on("error", (error) => this.emit("error", error, guildId));
      this.#players.set(guildId, player);
    }
    return player;
  }

  /**
   * Passes on the gateway dispatches players need (the bot's voice state and voice server
   * updates); others are ignored. With discord.js: `client.on(Events.Raw, (p) => manager.handleRawEvent(p))`.
   *
   * Pass on `READY` too (it arrives before `clientReady`, so listen before logging in): it tells
   * the client which voice states are this process's. Discord sends the bot's voice states from
   * every session to every session, so without it a second instance of the bot, or another
   * shard process, takes over the voice connections of this one's players.
   */
  handleRawEvent(dispatch: GatewayDispatch): void {
    if (dispatch.t === "READY") {
      const d = dispatch.d as { session_id?: string };
      if (d.session_id) {
        this.#gatewaySessions.add(d.session_id);
      }
    } else if (dispatch.t === "VOICE_STATE_UPDATE") {
      const d = dispatch.d as {
        guild_id?: string;
        user_id: string;
        session_id: string;
        channel_id: string | null;
      };
      if (d.user_id === this.#userId && d.guild_id !== undefined) {
        const ours = this.#gatewaySessions.size === 0 || this.#gatewaySessions.has(d.session_id);
        this.#players.get(d.guild_id)?._handleVoiceState(d.session_id, d.channel_id, ours);
      }
    } else if (dispatch.t === "VOICE_SERVER_UPDATE") {
      const d = dispatch.d as { guild_id: string; token: string; endpoint: string | null };
      this.#players.get(d.guild_id)?._handleVoiceServer(d.token, d.endpoint);
    }
  }

  /** An HTTP request to the engine; a failure to get an answer is `connection_lost`. */
  async #http(path: string, init: RequestInit): Promise<Response> {
    try {
      return await this.connection.fetch(path, init);
    } catch (error) {
      throw new EntroximerError(
        "connection_lost",
        `no answer from the engine: ${error instanceof Error ? error.message : String(error)}`,
      );
    }
  }

  #onReady(info: ReadyInfo): void {
    if (this.#connectedBefore && !info.resumed && this.#players.size > 0) {
      // e.g. the engine restarted: its players are gone, so the bot leaves their channels too
      const lost = [...this.#players.values()];
      this.#players.clear();
      for (const player of lost) {
        if (player.channelId !== null) {
          this.#joinVoice(player.guildId, null, {});
        }
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
