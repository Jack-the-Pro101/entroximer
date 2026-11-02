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
| `/crossfade seconds` | Fades each track into the next over up to 12 s; 0 plays them back to back |
| `/pause`, `/resume` | Control playback |
| `/stop` | Stops playback; `/skip` continues with the queue |
| `/seek position` | Jumps to `90`, `1:30` or `1:02:03` |
| `/volume percent [layer]` | 0 to 1000; 100 leaves the audio unchanged. The music's, or the sound effects' or announcements' |
| `/effects [preset] [chain] [layer]` | Applies a preset (suggested as you type, e.g. `nightcore`, `bass_boost`, `karaoke`), a chain such as `lowpass(800) => reverb(10, 2)`, or a preset followed by a chain; with neither, removes effects. To the music, or to the sound effects or announcements |
| `/sfx play [file] [url] [loop] [volume]`, `/sfx stop` | Plays an attached audio file or a link over the music, up to 8 at once, on a loop if asked; stops them |
| `/announce [file] [url]` | Plays an announcement over the music, lowering it to a quarter meanwhile; announcements play one after another |
| `/nowplaying` | The track with its artwork, position, downloaded parts and what comes next |
| `/channel [channel] [silent]` | Moves playback messages to a channel, or turns silent on or off: only problems are posted, not what starts playing. Both last until the bot leaves. With neither, says where messages go |
| `/leave` | Stops and leaves the voice channel |
| `/engine` | The engine's versions, sources, YouTube session state and load |

The bot posts track starts, failures and the end of the queue in the player's text channel, and
DMs its owner when the engine's YouTube session expires. A player's text channel is the channel
of the command that made the bot join voice (or, if the bot cannot post there, the voice
channel's chat), and only `/channel` moves it; other commands, wherever they are used, leave it
alone. If the bot can no longer post there (the channel is deleted, or the bot loses access),
it says so in the voice channel's chat and posts there instead, or stops posting. A new player
starts over: neither its channel nor silent carries over from the last one.

The bot also needs permission to view and send messages in the channels it posts in.

## Smoke test

`bun run smoke <voice channel id> <track URL>` checks playback end to end without slash commands:
it joins the channel, plays the track, seeks, pauses and applies effects, plays clips over it on
layers of their own (one while the music is paused, one ducking it), then
plays a queue: a gapless transition to a queued track, a 6 s crossfade into another, a
SoundCloud search result skipped to and seeked, repeating a track, and skipping past the end.
The clips are beeps it uploads, if the engine takes uploads (`ENTROXIMER_UPLOADS=true`), so
that they are easy to hear; otherwise stretches of the track. It checks each step against what
the engine reports; with
`RUST_LOG=entroximer_server::node::player=debug`, the engine's log shows when it prepares each
next track.

## Troubleshooting

If commands fail with "Interaction has already been acknowledged", the bot warns that another
instance answered first, or it posts that another instance joined voice: the bot is running
twice with the same token, e.g. in a terminal that outlived its editor window. Discord sends
every command to both, both join voice, and each join ends the other's voice connection (close
code 4006), so nothing plays and `/leave` may leave the bot behind. Stop the other instance.
