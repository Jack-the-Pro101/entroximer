import type { ApiError } from "./generated/ApiError";
import type { ErrorCode } from "./generated/ErrorCode";

/**
 * Why something failed: one of the engine's error codes, or one raised by the client itself.
 *
 * - `connection_lost`: the connection dropped before the engine answered, so the request may or
 *   may not have taken effect.
 * - `timeout`: no answer in time.
 * - `closed`: the client was closed.
 * - `protocol_mismatch`: the engine speaks a different protocol version.
 * - `voice_conflict`: another gateway session of the same bot joined voice in the player's guild,
 *   which takes the voice connection from this one; usually the bot is running twice.
 */
export type ClientErrorCode =
  | ErrorCode
  | "connection_lost"
  | "timeout"
  | "closed"
  | "protocol_mismatch"
  | "voice_conflict";

export class EntroximerError extends Error {
  override readonly name = "EntroximerError";

  constructor(
    readonly code: ClientErrorCode,
    message: string,
  ) {
    super(message);
  }

  static fromApi(error: ApiError): EntroximerError {
    return new EntroximerError(error.code, error.message);
  }
}

/** `error` as an `EntroximerError`; anything else becomes an `internal` one. */
export function toEntroximerError(error: unknown): EntroximerError {
  if (error instanceof EntroximerError) {
    return error;
  }
  return new EntroximerError("internal", error instanceof Error ? error.message : String(error));
}
