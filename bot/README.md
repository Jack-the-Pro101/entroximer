# Development bot

A Discord bot for trying the engine through the client library: playback, the engine's
queue, search and effects.

## Setup

Copy `.env.example` to `.env` and set `BOT_TOKEN` (and `ENTROXIMER_URL` / `ENTROXIMER_PASSWORD`
if the engine is not on `ws://127.0.0.1:2333` without a password). Start the engine, then:

```sh
bun run register <guild id>   # makes the slash commands appear in that guild; again after they change
bun run start
```

The bot needs the `Guilds` and `Guild Voice States` intents (both unprivileged) and, in the
guild, permission to view, connect to and speak in voice channels.

## Commands

| Command | Does |
|---|---|
| `/join [channel]` | Joins your voice channel, or the one given |
| `/play query` | Plays a URL (Spotify links too, if the engine has a Spotify app), playlist or YouTube search, or queues it if something is playing; joins your channel if needed. Suggests search results as you type |
| `/next query` | Queues a track to play right after the current one |
| `/search query [source]` | Searches YouTube, YouTube Music, SoundCloud or Spotify (if the engine has a Spotify app), shows the results with their artwork, and queues the ones you pick |
| `/queue [page]` | Shows the queue, each track with its artwork |
| `/skip [to]` | Skips to the next track, or to a position in the queue |
| `/remove position`, `/move from to`, `/shuffle`, `/clear` | Edit the queue |
| `/repeat mode` | Repeats nothing, the current track, or the queue |
| `/pause`, `/resume` | Control playback |
| `/stop` | Stops playback; `/skip` continues with the queue |
| `/seek position` | Jumps to `90`, `1:30` or `1:02:03` |
| `/volume percent` | 0 to 1000; 100 leaves the audio unchanged |
| `/effects [preset] [chain]` | Applies a preset (suggested as you type, e.g. `nightcore`, `bass_boost`, `karaoke`), a chain such as `lowpass(800) => reverb(10, 2)`, or a preset followed by a chain; with neither, removes effects |
| `/nowplaying` | The track with its artwork, position, downloaded parts and what comes next |
| `/leave` | Stops and leaves the voice channel |
| `/engine` | The engine's versions, sources, YouTube session state and load |

The bot posts track starts, failures and the end of the queue in the channel of the latest
command, and DMs its owner when the engine's YouTube session expires.

## Smoke test

`bun run smoke <voice channel id> <track URL>` checks playback end to end without slash commands:
it joins the channel, plays the track, seeks, pauses and applies effects, then plays a queue: a
gapless transition to a queued track, a SoundCloud search result skipped to and seeked,
repeating a track, and skipping past the end. It checks each step against what the engine
reports; with `RUST_LOG=entroximer_server::node::player=debug`, the engine's log shows when it
prepares each next track.

## Troubleshooting

If commands fail with "Interaction has already been acknowledged", the bot warns that another
instance answered first, or it posts that another instance joined voice: the bot is running
twice with the same token, e.g. in a terminal that outlived its editor window. Discord sends
every command to both, both join voice, and each join ends the other's voice connection (close
code 4006), so nothing plays and `/leave` may leave the bot behind. Stop the other instance.
