# Entroximer

Lavalink but in Rust: a Discord audio engine with seekable streaming, effect graphs and presets,
YouTube and Spotify support, and a TypeScript client.

| Directory | Contents |
|---|---|
| `engine/` | The Rust engine; see [engine/README.md](engine/README.md) |
| `client/` | The TypeScript client library |
| `bot/` | A Discord bot for testing the engine through the client |

## Development

With Nix, `nix develop` (or `nix-shell`) provides everything: the Rust toolchain, ffmpeg, yt-dlp,
Deno and Bun. `nix build` builds the engine (`result/bin/entroximer`).

```sh
bun install
(cd engine && cargo build && cargo test --workspace)   # the engine
(cd client && bun test && bun run typecheck)           # the client, against the built engine
(cd bot && bun run start)                              # the bot; see bot/README.md
```
