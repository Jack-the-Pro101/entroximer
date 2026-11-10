// Starts a real engine for the integration tests: the binary from `cargo build` (or
// `$ENTROXIMER_BIN`), with a test tone to play. Tests skip themselves when it is missing.
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { connect, createServer } from "node:net";
import type { AddressInfo, Socket } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

const BIN =
  process.env.ENTROXIMER_BIN ??
  new URL("../../engine/target/debug/entroximer", import.meta.url).pathname;

export const canRunEngine = existsSync(BIN) && Bun.which("ffmpeg") !== null;

export interface TestEngine {
  url: string;
  /** A 30 s stereo tone in Opus/WebM. */
  tone: string;
  /** A short MP3 with a cover embedded. */
  covered: string;
  stop(): Promise<void>;
}

export async function startEngine(options: { password?: string; uploads?: boolean } = {}): Promise<TestEngine> {
  const dir = mkdtempSync(join(tmpdir(), "entroximer-client-test-"));
  const tone = join(dir, "tone.webm");
  const ffmpeg = Bun.spawnSync([
    "ffmpeg", "-nostdin", "-loglevel", "error", "-y",
    "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=30",
    "-ac", "2", "-c:a", "libopus", tone,
  ]);
  if (!ffmpeg.success) {
    throw new Error(`ffmpeg failed: ${ffmpeg.stderr.toString()}`);
  }
  const covered = join(dir, "covered.mp3");
  const cover = Bun.spawnSync([
    "ffmpeg", "-nostdin", "-loglevel", "error", "-y",
    "-f", "lavfi", "-i", "sine=frequency=440:duration=3",
    "-f", "lavfi", "-i", "color=c=red:s=300x300:d=1",
    "-map", "0:a", "-map", "1:v", "-frames:v", "1",
    "-c:a", "libmp3lame", "-c:v", "mjpeg", "-disposition:v", "attached_pic", covered,
  ]);
  if (!cover.success) {
    throw new Error(`ffmpeg failed: ${cover.stderr.toString()}`);
  }

  const port = freePort();
  const engine = Bun.spawn([BIN, "serve"], {
    env: {
      ...process.env,
      ENTROXIMER_PORT: String(port),
      ENTROXIMER_PASSWORD: options.password ?? "",
      ENTROXIMER_DATA_DIR: join(dir, "data"),
      ENTROXIMER_CACHE_DIR: join(dir, "cache"),
      ENTROXIMER_YOUTUBE_CHECK_HOURS: "0",
      ENTROXIMER_PLAYER_UPDATE_SECS: "1",
      // the tone is a local file
      ENTROXIMER_ALLOW_LOCAL_FILES: "true",
      // longer than the tone, so that a queue's next track is loaded right away
      ENTROXIMER_PRELOAD_SECS: "60",
      ENTROXIMER_UPLOADS: String(options.uploads ?? false),
      ENTROXIMER_UPLOAD_MAX_MB: "1",
      RUST_LOG: "warn",
    },
    stdout: "ignore",
    stderr: "inherit",
  });

  const deadline = Date.now() + 10_000;
  for (;;) {
    try {
      if ((await fetch(`http://127.0.0.1:${port}/health`)).ok) {
        break;
      }
    } catch {
      // not listening yet
    }
    if (Date.now() > deadline || engine.exitCode !== null) {
      engine.kill();
      throw new Error("the engine did not start");
    }
    await Bun.sleep(50);
  }

  return {
    url: `ws://127.0.0.1:${port}`,
    tone,
    covered,
    async stop() {
      engine.kill();
      await engine.exited;
      rmSync(dir, { recursive: true, force: true });
    },
  };
}

export interface Relay {
  /** The engine's URL by way of the relay. */
  url: string;
  /**
   * Stops passing anything on for the connections open now, in either direction and without
   * closing them: what a network that went away looks like to both ends.
   */
  freeze(): void;
  stop(): void;
}

/** A TCP relay in front of the engine at `url`. */
export async function startRelay(url: string): Promise<Relay> {
  const engine = new URL(url);
  let live = new Set<Socket>();
  const all = new Set<Socket>();
  const server = createServer((client) => {
    const upstream = connect({ host: engine.hostname, port: Number(engine.port) });
    for (const [from, to] of [
      [client, upstream],
      [upstream, client],
    ] as const) {
      all.add(from);
      live.add(from);
      from.on("data", (chunk) => {
        if (live.has(from)) {
          to.write(chunk);
        }
      });
      from.on("close", () => {
        all.delete(from);
        if (live.delete(from)) {
          to.destroy();
        }
      });
      from.on("error", () => {});
    }
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  return {
    url: `ws://127.0.0.1:${(server.address() as AddressInfo).port}`,
    freeze() {
      live = new Set();
    },
    stop() {
      server.close();
      for (const socket of all) {
        socket.destroy();
      }
    },
  };
}

function freePort(): number {
  const server = Bun.serve({ port: 0, fetch: () => new Response() });
  const port = server.port;
  server.stop(true);
  if (port === undefined) {
    throw new Error("no free port");
  }
  return port;
}

/** Resolves with the arguments of the next call to the returned listener. */
export function nextCall<Args extends unknown[]>(
  timeoutMs = 10_000,
): { listener: (...args: Args) => void; called: Promise<Args> } {
  let resolve!: (args: Args) => void;
  const called = new Promise<Args>((res, reject) => {
    resolve = res;
    setTimeout(() => reject(new Error("timed out waiting for a call")), timeoutMs);
  });
  return { listener: (...args) => resolve(args), called };
}
