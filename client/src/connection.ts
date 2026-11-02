import { Emitter } from "./emitter";
import { EntroximerError } from "./errors";
import type { Call } from "./generated/Call";
import type { Event } from "./generated/Event";
import type { ClipInfo } from "./generated/ClipInfo";
import type { GraphSpec } from "./generated/GraphSpec";
import type { LayerInfo } from "./generated/LayerInfo";
import type { LoadResult } from "./generated/LoadResult";
import type { NodeInfo } from "./generated/NodeInfo";
import type { PlayerInfo } from "./generated/PlayerInfo";
import type { QueueAdded } from "./generated/QueueAdded";
import type { QueuePage } from "./generated/QueuePage";
import type { QueueState } from "./generated/QueueState";
import type { SearchResult } from "./generated/SearchResult";
import type { ServerMessage } from "./generated/ServerMessage";
import type { SessionSettings } from "./generated/SessionSettings";
import type { Stats } from "./generated/Stats";
import type { Track } from "./generated/Track";

/** The protocol version this client speaks; see `node.info`. */
export const PROTOCOL_VERSION = 1;

const CLIENT_NAME = "entroximer-client/0.1.0";

/** Every request the engine understands, by `op`. */
export type Op = Call["op"];

/** What each request resolves to. */
export interface Results {
  "tracks.load": LoadResult;
  "tracks.search": SearchResult;
  "tracks.decode": Track[];
  "presets.build": GraphSpec;
  "player.get": PlayerInfo;
  "player.update": PlayerInfo;
  "player.destroy": null;
  "queue.get": QueuePage;
  "queue.add": QueueAdded;
  "queue.remove": QueueState;
  "queue.move": QueueState;
  "queue.clear": QueueState;
  "queue.shuffle": QueueState;
  "queue.skip": PlayerInfo;
  "queue.configure": QueueState;
  "layer.update": LayerInfo;
  "layer.remove": null;
  "clip.play": ClipInfo;
  "clip.stop": LayerInfo;
  "session.configure": SessionSettings;
  "node.info": NodeInfo;
  "node.stats": Stats;
}

/** The parameters of `op`, as a tuple: empty for requests without parameters. */
export type ParamsOf<O extends Op> =
  Extract<Call, { op: O }> extends { params: infer P } ? [params: P] : [];

/** Sends a request and resolves with its result; see `Connection.request`. */
export type Requester = <O extends Op>(op: O, ...params: ParamsOf<O>) => Promise<Results[O]>;

export interface ConnectionOptions {
  /** The engine, e.g. `ws://127.0.0.1:2333`. `http(s)` works too; the path defaults to `/v1/ws`. */
  url: string;
  /** The bot's Discord user ID. */
  userId: string;
  /** The engine's password, if it has one. */
  password?: string;
  /** Identifies the client in the engine's logs, as `name/version`. */
  clientName?: string;
  /** How long a request may take, including waiting for a reconnect. Default 30 s. */
  requestTimeoutMs?: number;
  /** Delay before reconnecting, doubling per failed attempt up to the maximum. Default 1 s to 30 s. */
  reconnectDelayMs?: { min: number; max: number };
}

export interface ReadyInfo {
  sessionId: string;
  /** Whether the previous session was resumed, so its players continue where they paused. */
  resumed: boolean;
  /** Events from while disconnected that the engine could not keep. */
  missedEvents: number;
}

export interface DisconnectInfo {
  code: number;
  reason: string;
  /** Whether the client will try to reconnect and resume the session. */
  willReconnect: boolean;
}

type ConnectionEvents = {
  ready: [info: ReadyInfo];
  event: [event: Event];
  disconnect: [info: DisconnectInfo];
};

interface Pending {
  resolve: (result: unknown) => void;
  reject: (error: EntroximerError) => void;
  timer: ReturnType<typeof setTimeout>;
}

/**
 * The WebSocket to the engine. Requests wait for (re)connection, responses are matched to their
 * requests, and a dropped connection is reopened with the same session, so players continue
 * where they paused.
 */
export class Connection extends Emitter<ConnectionEvents> {
  /**
   * The engine sends `stats` every minute; this much silence means the connection is dead even
   * if the socket has not noticed.
   */
  static readonly #SILENCE_LIMIT_MS = 75_000;

  readonly #url: string;
  /** The engine's HTTP origin, for what it serves outside the WebSocket. */
  readonly #httpOrigin: string;
  readonly #headers: Record<string, string>;
  readonly #password: string | undefined;
  readonly #requestTimeoutMs: number;
  readonly #reconnectDelay: { min: number; max: number };

  #socket: WebSocket | null = null;
  #ready = false;
  #sessionId: string | null = null;
  #nextId = 1;
  readonly #pending = new Map<number, Pending>();
  readonly #readyWaiters = new Set<() => void>();
  #closed = false;
  #failedAttempts = 0;
  #reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  #silenceTimer: ReturnType<typeof setTimeout> | null = null;

  constructor(options: ConnectionOptions) {
    super();
    this.#url = websocketUrl(options.url);
    this.#httpOrigin = httpOrigin(options.url);
    this.#password = options.password;
    this.#headers = {
      "User-Id": options.userId,
      "Client-Name": options.clientName ?? CLIENT_NAME,
      ...(options.password === undefined ? {} : { Authorization: options.password }),
    };
    this.#requestTimeoutMs = options.requestTimeoutMs ?? 30_000;
    this.#reconnectDelay = options.reconnectDelayMs ?? { min: 1_000, max: 30_000 };
  }

  /** The engine's ID for this client's session, once connected. */
  get sessionId(): string | null {
    return this.#sessionId;
  }

  get isReady(): boolean {
    return this.#ready;
  }

  /**
   * Connects and waits for the engine's `ready`. Rejects if this first attempt fails, e.g. for
   * a wrong URL or password; after that, dropped connections are reopened automatically.
   */
  connect(): Promise<ReadyInfo> {
    if (this.#closed) {
      return Promise.reject(new EntroximerError("closed", "the connection was closed"));
    }
    return new Promise((resolve, reject) => {
      const removeReady = this.once("ready", (info) => {
        removeDisconnect();
        resolve(info);
      });
      const removeDisconnect = this.once("disconnect", ({ code, reason }) => {
        removeReady();
        this.close();
        reject(
          new EntroximerError(
            "connection_lost",
            `could not connect to the engine at ${this.#url} (${code}${reason ? `: ${reason}` : ""}); ` +
              "check the URL, and the password if the engine has one",
          ),
        );
      });
      this.#open();
    });
  }

  /** Closes the connection for good; pending requests fail with `closed`. */
  close(): void {
    this.#closed = true;
    this.#clearTimers();
    this.#socket?.close(1000, "client closed");
    this.#socket = null;
    this.#ready = false;
    this.#failAll(new EntroximerError("closed", "the connection was closed"));
  }

  /** Reopens the connection, resuming the session; e.g. after the network changed. */
  reconnect(): void {
    this.#socket?.close(4000, "reconnecting");
  }

  /** The absolute URL of `path` on the engine's HTTP side, e.g. `/v1/artwork/...`. */
  httpUrl(path: string): string {
    return new URL(path, this.#httpOrigin).toString();
  }

  /** Fetches `path` from the engine over HTTP, with the engine's password if it has one. */
  fetch(path: string, init: RequestInit = {}): Promise<Response> {
    const headers = new Headers(init.headers);
    if (this.#password !== undefined) {
      headers.set("Authorization", this.#password);
    }
    return fetch(this.httpUrl(path), { ...init, headers });
  }

  /** Sends a request and resolves with its result. */
  async request<O extends Op>(op: O, ...params: ParamsOf<O>): Promise<Results[O]> {
    if (this.#closed) {
      throw new EntroximerError("closed", "the connection was closed");
    }
    const deadline = Date.now() + this.#requestTimeoutMs;
    // the connection can drop again between waking up and sending
    while (!this.#ready) {
      await this.#waitUntilReady(deadline);
    }

    const id = this.#nextId++;
    const message = JSON.stringify(params.length ? { id, op, params: params[0] } : { id, op });
    return new Promise<Results[O]>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.#pending.delete(id);
        reject(new EntroximerError("timeout", `\`${op}\` got no answer in time`));
      }, Math.max(0, deadline - Date.now()));
      this.#pending.set(id, { resolve: resolve as (result: unknown) => void, reject, timer });
      this.#socket?.send(message);
    });
  }

  #open(): void {
    const headers = { ...this.#headers };
    if (this.#sessionId !== null) {
      headers["Session-Id"] = this.#sessionId;
    }
    const socket = new WebSocket(this.#url, { headers });
    this.#socket = socket;
    socket.addEventListener("message", (message) => {
      if (this.#socket === socket && typeof message.data === "string") {
        this.#watchSilence();
        this.#handle(JSON.parse(message.data) as ServerMessage);
      }
    });
    socket.addEventListener("close", ({ code, reason }) => {
      if (this.#socket === socket) {
        this.#onClose(code, reason);
      }
    });
  }

  #handle(message: ServerMessage): void {
    switch (message.type) {
      case "ready": {
        this.#ready = true;
        this.#sessionId = message.sessionId;
        this.#failedAttempts = 0;
        for (const wake of [...this.#readyWaiters]) {
          wake();
        }
        this.emit("ready", {
          sessionId: message.sessionId,
          resumed: message.resumed,
          missedEvents: message.missedEvents,
        });
        break;
      }
      case "response": {
        const pending = this.#pending.get(message.id);
        if (!pending) {
          return; // timed out meanwhile
        }
        this.#pending.delete(message.id);
        clearTimeout(pending.timer);
        if ("error" in message) {
          pending.reject(EntroximerError.fromApi(message.error));
        } else {
          pending.resolve(message.result);
        }
        break;
      }
      case "event": {
        const { type: _, ...event } = message;
        this.emit("event", event as Event);
        break;
      }
    }
  }

  #onClose(code: number, reason: string): void {
    const wasReady = this.#ready;
    this.#socket = null;
    this.#ready = false;
    this.#clearTimers();
    this.#failAll(
      new EntroximerError("connection_lost", "the connection dropped before the engine answered"),
    );

    // only reconnect connections that worked before; a first failure is reported by `connect`
    const willReconnect = !this.#closed && (wasReady || this.#failedAttempts > 0);
    this.emit("disconnect", { code, reason, willReconnect });
    if (willReconnect) {
      const delay = Math.min(
        this.#reconnectDelay.max,
        this.#reconnectDelay.min * 2 ** this.#failedAttempts,
      );
      this.#failedAttempts++;
      this.#reconnectTimer = setTimeout(() => this.#open(), delay * (0.8 + Math.random() * 0.4));
    }
  }

  #waitUntilReady(deadline: number): Promise<void> {
    if (this.#ready) {
      return Promise.resolve();
    }
    return new Promise((resolve, reject) => {
      const wake = () => {
        clearTimeout(timer);
        this.#readyWaiters.delete(wake);
        resolve();
      };
      const timer = setTimeout(() => {
        this.#readyWaiters.delete(wake);
        reject(new EntroximerError("timeout", "not connected to the engine"));
      }, Math.max(0, deadline - Date.now()));
      this.#readyWaiters.add(wake);
    });
  }

  #watchSilence(): void {
    if (this.#silenceTimer !== null) {
      clearTimeout(this.#silenceTimer);
    }
    this.#silenceTimer = setTimeout(
      () => this.#socket?.close(4001, "the engine went silent"),
      Connection.#SILENCE_LIMIT_MS,
    );
  }

  #clearTimers(): void {
    for (const timer of [this.#reconnectTimer, this.#silenceTimer]) {
      if (timer !== null) {
        clearTimeout(timer);
      }
    }
    this.#reconnectTimer = null;
    this.#silenceTimer = null;
  }

  #failAll(error: EntroximerError): void {
    for (const pending of this.#pending.values()) {
      clearTimeout(pending.timer);
      pending.reject(error);
    }
    this.#pending.clear();
  }
}

/** The HTTP origin of the engine at `url`, whether given as `ws(s)://` or `http(s)://`. */
function httpOrigin(url: string): string {
  const parsed = new URL(url);
  parsed.protocol = parsed.protocol.replace(/^ws/, "http");
  return parsed.origin;
}

/** `http(s)://` becomes `ws(s)://`, and an empty path becomes `/v1/ws`. */
function websocketUrl(url: string): string {
  const parsed = new URL(url);
  parsed.protocol = parsed.protocol.replace(/^http/, "ws");
  if (parsed.pathname === "" || parsed.pathname === "/") {
    parsed.pathname = "/v1/ws";
  }
  return parsed.toString();
}
