// Settings from the environment; Bun loads `.env` automatically (see `.env.example`).

function required(name: string): string {
  const value = process.env[name];
  if (!value) {
    throw new Error(`Missing required environment variable ${name}; see .env.example`);
  }
  return value;
}

export const env = Object.freeze({
  BOT_TOKEN: required("BOT_TOKEN"),
  /** The engine's address. */
  ENTROXIMER_URL: process.env.ENTROXIMER_URL || "ws://127.0.0.1:2333",
  /** The engine's password, if it has one. */
  ENTROXIMER_PASSWORD: process.env.ENTROXIMER_PASSWORD || undefined,
});
