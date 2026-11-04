# NodeLink: what could help Entroximer

Notes from reading [PerformanC/NodeLink](https://github.com/PerformanC/NodeLink) (commit `804e0d6`,
October 2026), ranked by how much each idea would add here, with where it would fit.

**License:** NodeLink is GPL-3.0-or-later, and several of its DSP files ask that ports stay GPL.
None of its code is in Entroximer. The presets, nodes and Spotify matching were written from scratch
with standard techniques; only the ideas are borrowed. Keep it that way unless Entroximer
adopts the GPL.

## Worth doing next

1. **Loudness normalization** (`LoudnessNormalizer`): K-weighted loudness measured while playing,
   steering towards -14 LUFS, so that a quiet upload and a loud master play at the same volume.
   Probably the single most noticeable improvement for listeners. It fits as a per-track gain
   in the deck (measured on each track's PCM before the effects). A "loudness" preset node would
   be the wrong place, because it must reset per track.
2. **Smooth effect changes** (`AnimatableFilter`): every filter fades between old and new
   parameters over a client-given `transition` (`linear`, `exponential`, `sinusoidal`). Here,
   setting a graph swaps the compiled `Effects` at once, which can click. A short crossfade
   between the old and new `Effects` in `Deck::set_effects` (render both for ~50 ms and mix) would
   fix that generically, for every node and preset, without per-node animation code.
3. **Crossfades and DJ-style transitions** (`CrossfadeController`, `MusicalAnalyzer`,
   `AutoMixRegistry`): overlap the end of one track with the start of the next, optionally
   beat-matched (BPM, downbeat, key on the Camelot wheel) with EQ swaps. *Done without the
   beat-matching:* `queue.configure { crossfade }`, an equal-power overlap in the deck.
4. **Mix layers** (`AudioMixer`, `sessions/{id}/players/{id}/mix`): play extra streams (sound
   effects, TTS announcements) over the music without stopping it, up to N layers, each with
   its own volume. *Done, as layers mixed after each one's own effects:* summing them before
   the music's effects, as first noted here, would put an announcement through the music's
   nightcore preset and volume. `layer.update` and `clip.play`, with clips that queue, mix or
   replace each other, ducking, and a limiter on the sum (`crates/entroximer-core/src/mixer.rs`).
5. **SponsorBlock and chapters**: skip the intro, outro and non-music segments of YouTube music
   videos, and report chapters to clients. yt-dlp already exposes `chapters` (which
   `slim_info_json` currently strips) and has SponsorBlock support (`--sponsorblock-*`), so this
   is mostly metadata plumbing plus `end_at` and seeks.
6. **Lyrics** (LRCLIB, Musixmatch, YouTube, Genius, Deezer, ...), including a subscription that
   pushes synced lines as the track plays. LRCLIB is free and open. Synced lyrics as events would
   fit the single-WebSocket protocol nicely.

## Useful for operating the engine

- **Rate limiting and admission control** (`RateLimitManager`, `DosProtectionManager`,
  `AdmissionManager`): token buckets per IP, user, guild and connection, and limits on players
  per session. Entroximer has only the password today.
- **Prometheus metrics** (`/metrics`): players, cache usage, yt-dlp runs, error codes. `node.stats`
  already gathers most of it.
- **Route planner** (`RoutePlannerManager`): rotating outgoing IPv6 addresses (`RotateOnBan`,
  `RoundRobin`, `LoadBalance`) when YouTube rate-limits one. yt-dlp takes `--source-address`, so
  the managed runs could rotate through a configured block.
- **Persistent track cache** (`TrackCacheManager`, AES-GCM on disk): resolved tracks survive
  restarts. Less important here, since media URLs expire within hours; the Spotify mirror
  mapping (track ID to YouTube video) would be the part worth persisting.

## More sources

- **Mirrored catalogs** like Spotify: Apple Music, Deezer, Tidal, Amazon Music, Yandex, JioSaavn,
  Anghami and others, all played from YouTube. With the `Provider` trait and
  `spotify/mirror.rs` (searches plus matching), each is mostly a metadata client.
- **Songlink / Odesli** (`songlink.ts`): turns a link from any service into the matching links
  on the others, which is a cheap way to accept Apple Music, Tidal or Deezer links without a
  client for each.
- **Text to speech** (Google TTS, Piper, Flowery): announcements, especially paired with mix
  layers.
- **Recommendations / autoplay**: NodeLink gets Spotify recommendations through Spotify's
  internal web-player API. The official API removed recommendations in 2024, so an autoplay
  would rather come from YouTube Music's "up next" (yt-dlp can list a video's mix playlist,
  `list=RD<id>`).

## Effects not covered yet

- **Phonograph** goes beyond our preset: early reflections, mic AGC and crackle noise. A
  `noise`/`crackle` source node would make vinyl presets more convincing.
- **Spatial** (cross-channel delays with modulation) and **tesseract** (NodeLink's own
  multi-dimensional rotation) are further stereo-movement effects; `auto_pan` plus `reverb`
  covers the common "8D" case.
- **Time stretching** (NodeLink's `timescale` speed and pitch): we have `rate` (speed and pitch
  together) and `pitch` (a delay-line shifter, fine for effects, smeary on chords). A WSOLA time
  stretcher would give clean "faster at the same pitch" at the cost of more DSP.
- **Tape stop, scratch, fades** (`TapeTransformer`, `ScratchTransformer`, `FadeTransformer`): fun
  transitions. A tape stop is a rate ramp to zero, which `rubato`'s `set_resample_ratio` can do
  on the existing rate stage.
- **Ducking** (`DuckingController`): lowers the music while people in the channel speak. It needs
  voice receive (songbird can decode incoming audio), which costs CPU and bandwidth per guild.

## Deliberately not copied

- NodeLink reaches Spotify mostly through the web player's anonymous token and its internal
  GraphQL API, which avoids the official API's limits but goes against Spotify's developer
  terms and breaks when the web player changes. Entroximer uses the official Web API with the
  operator's app credentials, and documents the development-mode limits instead.
