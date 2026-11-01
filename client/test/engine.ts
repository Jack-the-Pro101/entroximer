// Starts a real engine for the integration tests: the binary from `cargo build` (or
// `$ENTROXIMER_BIN`), with a test tone to play. Tests skip themselves when it is missing.
import { existsSync, mkdtempSync, rmSync } from "node:fs";
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
  stop(): Promise<void>;
}

export async function startEngine(options: { password?: string } = {}): Promise<TestEngine> {
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
    async stop() {
      engine.kill();
      await engine.exited;
      rmSync(dir, { recursive: true, force: true });
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
