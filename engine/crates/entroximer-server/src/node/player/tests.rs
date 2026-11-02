//! The queue against a mixer rendered by hand through [`MixerSource`], as songbird's mixer renders
//! it once voice connects, but faster than real time. Tracks are two-second tones in local files,
//! named after their titles.

use std::collections::HashMap;
use std::io::Read;
use std::time::Instant;

use clap::Parser;
use serde_json::Value;

use super::*;
use crate::api::protocol::{ServerMessage, TrackSelector};
use crate::config::{MediaConfig, MediaContext};
use crate::node::NodeSettings;
use crate::node::output::MixerSource;
use crate::node::tracks;
use crate::sources::{FoundTrack, TrackInfo, TrackRef};

const TONE: Duration = Duration::from_secs(2);
/// Just past a tone's end.
const WHOLE_TONE: Duration = Duration::from_millis(2100);
/// What the mixer reads at a time: 20 ms of interleaved stereo f32.
const FILL: Duration = Duration::from_millis(20);
const FILL_BYTES: usize = 960 * 2 * 4;

#[derive(Parser)]
struct Cli {
    #[command(flatten)]
    media: MediaConfig,
}

/// songbird's shared scheduler runs on the runtime that first asks for it. Each test has its own
/// runtime, which ends with the test, so the scheduler is made first on one that never does.
fn keep_songbird_scheduler() {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    let runtime = RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
    });
    let _entered = runtime.enter();
    songbird::driver::get_default_scheduler();
}

struct Harness {
    node: Arc<Node>,
    player: Arc<GuildPlayer>,
    output: MixerSource,
    buffer: Vec<u8>,
    messages: mpsc::UnboundedReceiver<ServerMessage>,
    /// Item ids by the names events are described with.
    names: HashMap<String, String>,
    /// The last queue version seen.
    version: u64,
    dir: tempfile::TempDir,
    _session: Arc<Session>,
}

impl Harness {
    async fn new(preload_lead: Duration) -> Self {
        Self::build(preload_lead, Duration::from_secs(60)).await
    }

    async fn build(preload_lead: Duration, stuck_after: Duration) -> Self {
        keep_songbird_scheduler();
        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| dir.path().join(name).to_string_lossy().into_owned();
        let cli = Cli::parse_from([
            "entroximer",
            "--data-dir",
            &path("data"),
            "--cache-dir",
            &path("cache"),
            "--allow-local-files",
            "--allow-private-urls",
        ]);
        let node = Node::new(
            Arc::new(MediaContext::new(cli.media).unwrap()),
            NodeSettings {
                password: None,
                player_update_interval: Duration::from_secs(3600),
                stuck_after,
                resume_timeout: Duration::from_secs(60),
                preload_lead,
            },
        );
        let (session, _) = node.open_session(Snowflake(1), None);
        let (out, messages) = mpsc::unbounded_channel();
        session.attach(out, false);
        let player = session.player_or_create(&node, Snowflake(2));
        let output = MixerSource::new(Arc::clone(&player.mixer), player.signals.clone());
        Self {
            node,
            player,
            output,
            buffer: vec![0; FILL_BYTES],
            messages,
            names: HashMap::new(),
            version: 0,
            dir,
            _session: session,
        }
    }

    /// Tracks for tones, loaded as clients load them.
    async fn tones(&self, names: &[&str]) -> Vec<Track> {
        let mut tracks = Vec::new();
        for name in names {
            let path = self.tone_file(name).await;
            let selector = TrackSelector::Query {
                query: path,
                user_data: None,
            };
            tracks.extend(self.node.queue_tracks(&selector).await.unwrap());
        }
        tracks
    }

    async fn tone_file(&self, name: &str) -> String {
        let path = self.dir.path().join(format!("{name}.webm"));
        if !path.exists() {
            let status = tokio::process::Command::new("ffmpeg")
                .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"])
                .args(["-f", "lavfi", "-i"])
                .arg(format!(
                    "sine=frequency=440:sample_rate=48000:duration={}",
                    TONE.as_secs()
                ))
                .args(["-ac", "2", "-c:a", "libopus"])
                .arg(&path)
                .status()
                .await
                .expect("ffmpeg must be installed to run these tests");
            assert!(status.success());
        }
        path.to_string_lossy().into_owned()
    }

    async fn add(&mut self, tracks: Vec<Track>, index: Option<u32>) {
        let added = self.player.queue_add(tracks, index).await.unwrap();
        for item in added.items {
            self.names.insert(item.id, item.track.info.title);
        }
    }

    /// The title of the prepared next track.
    fn prepared(&self) -> Option<String> {
        let state = self.player.state();
        let (track, _) = state.prepared.as_ref()?;
        Some(state.items[track].track.info.title.clone())
    }

    fn queue(&self) -> Vec<String> {
        self.player
            .queue_page(0, usize::MAX)
            .items
            .iter()
            .map(|item| self.names[&item.id].clone())
            .collect()
    }

    /// What a preload tick does, ticking with every fill.
    fn tick(&self) {
        self.player.plan_next(&mut self.player.state());
    }

    /// Plays `duration` of audio, waiting out buffering as a listener would; returns the events
    /// of each fill that had any.
    async fn play(&mut self, duration: Duration) -> Vec<Vec<String>> {
        let mut fills = duration.as_millis() / FILL.as_millis();
        let mut batches = Vec::new();
        while fills > 0 {
            self.output.read_exact(&mut self.buffer).unwrap();
            // of the music or a clip
            let buffering = self
                .player
                .mixer()
                .players()
                .any(|player| player.buffering_since().is_some());
            if buffering {
                tokio::time::sleep(Duration::from_millis(2)).await;
            } else {
                fills -= 1;
            }
            self.tick();
            // in real time, a preparation has many seconds to finish
            self.until("the preparation finishes", |p| {
                p.state().preparing.is_none()
            })
            .await;
            let batch = self.events().await;
            if !batch.is_empty() {
                batches.push(batch);
            }
        }
        batches
    }

    /// Waits until the prepared next track has downloaded to its end, as it has long before a
    /// crossfade in real time.
    async fn next_downloaded(&self) {
        self.until("the next track downloads", |p| {
            p.deck()
                .next()
                .is_some_and(|next| next.player.media_end().is_some())
        })
        .await;
    }

    async fn until(&self, what: &str, done: impl Fn(&GuildPlayer) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(&self.player) {
            assert!(Instant::now() < deadline, "timed out waiting until {what}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// The events reported so far, once the player's event task has caught up.
    async fn events(&mut self) -> Vec<String> {
        self.player.flush().await;
        let mut events = Vec::new();
        while let Ok(message) = self.messages.try_recv() {
            events.extend(self.describe(message));
        }
        events
    }

    /// Events up to and including the first `last`, and any that follow right away.
    async fn events_until(&mut self, last: &str) -> Vec<String> {
        let mut events = Vec::new();
        while !events.iter().any(|e: &String| e.starts_with(last)) {
            let message = tokio::time::timeout(Duration::from_secs(10), self.messages.recv())
                .await
                .unwrap_or_else(|_| panic!("no `{last}` within 10 s, only {events:?}"))
                .unwrap();
            events.extend(self.describe(message));
        }
        events.extend(self.events().await);
        events
    }

    /// An event in short, with items by name; checks that queue versions count up by one.
    fn describe(&mut self, message: ServerMessage) -> Option<String> {
        let ServerMessage::Event(event) = message else {
            return None;
        };
        let event = serde_json::to_value(event).unwrap();
        let name = |id: &Value| {
            self.names
                .get(id.as_str().unwrap())
                .cloned()
                .unwrap_or_else(|| format!("unknown item {id}"))
        };
        let names = |ids: &Value| {
            ids.as_array()
                .unwrap()
                .iter()
                .map(name)
                .collect::<Vec<_>>()
                .join(" ")
        };
        let text = |value: &Value| value.as_str().unwrap().to_owned();
        Some(match event["event"].as_str().unwrap() {
            "trackStart" => format!("start {}", name(&event["itemId"])),
            "trackEnd" => format!("end {} {}", name(&event["itemId"]), text(&event["reason"])),
            "trackError" => format!("error {}", name(&event["itemId"])),
            "queueChanged" => {
                let version = event["version"].as_u64().unwrap();
                assert_eq!(version, self.version + 1, "a queue version was skipped");
                self.version = version;
                let change = &event["change"];
                let detail = match change["type"].as_str().unwrap() {
                    "added" => {
                        let ids: Vec<Value> = change["items"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|item| item["id"].clone())
                            .collect();
                        names(&Value::from(ids))
                    }
                    "removed" => names(&change["ids"]),
                    "moved" | "advanced" => name(&change["id"]),
                    "updated" => text(&change["item"]["track"]["info"]["title"]),
                    "repeat" => text(&change["repeat"]),
                    _ => String::new(),
                };
                format!("{} {detail}", text(&change["type"]))
                    .trim_end()
                    .to_owned()
            }
            "clipStart" => format!("clip start {}", text(&event["clipId"])),
            "clipEnd" => format!(
                "clip end {} {}",
                text(&event["clipId"]),
                text(&event["reason"])
            ),
            "clipError" => format!("clip error {}", text(&event["clipId"])),
            "clipStuck" => format!("clip stuck {}", text(&event["clipId"])),
            "playerUpdate" | "stats" => return None,
            other => other.to_owned(),
        })
    }
}

/// A track that cannot be resolved: a local file that does not exist.
fn missing(name: &str) -> Track {
    found(
        name,
        TrackRef {
            source: "local".into(),
            uri: format!("/nonexistent/{name}.webm"),
        },
    )
}

fn found(title: &str, reference: TrackRef) -> Track {
    let info = TrackInfo {
        identifier: reference.uri.clone(),
        title: title.to_owned(),
        author: None,
        author_url: None,
        album: None,
        length: None,
        is_stream: false,
        is_seekable: true,
        uri: reference.uri.clone(),
        artwork_url: None,
        isrc: None,
        source_name: reference.source.clone(),
    };
    tracks::to_track(FoundTrack { reference, info })
}

fn strings<const N: usize>(texts: [&str; N]) -> Vec<String> {
    texts.into_iter().map(str::to_owned).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_queue_plays_through_without_gaps() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    let tracks = h.tones(&["a", "b", "c"]).await;
    h.add(tracks, None).await;
    // with nothing playing, the first item starts and leaves the queue
    assert_eq!(
        h.events_until("start").await,
        strings(["added a b c", "advanced a", "start a"])
    );

    h.play(Duration::from_millis(800)).await;
    assert_eq!(h.prepared(), None, "1.2 s remain, more than the lead");
    h.play(Duration::from_millis(400)).await;
    assert_eq!(h.prepared().as_deref(), Some("b"), "0.8 s remain");
    // the next track starts in the same fill as the current one ends
    let batches = h.play(Duration::from_millis(900)).await;
    assert_eq!(
        batches,
        [strings(["end a finished", "advanced b", "start b"])]
    );

    let batches = h.play(WHOLE_TONE).await;
    assert_eq!(
        batches,
        [strings(["end b finished", "advanced c", "start c"])]
    );
    let batches = h.play(WHOLE_TONE).await;
    assert_eq!(batches, [strings(["end c finished", "queueEnd"])]);
    assert!(h.player.deck().is_idle());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crossfade_switches_tracks_where_it_starts() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    let queue = h
        .player
        .configure_queue(None, Some(Millis(500)))
        .await
        .unwrap();
    assert_eq!(queue.crossfade, Millis(500));
    let tracks = h.tones(&["a", "b"]).await;
    h.add(tracks, None).await;
    assert_eq!(
        h.events_until("start").await,
        strings(["added a b", "advanced a", "start a"])
    );

    // the crossfade starts that much earlier, and so does preparing the next track
    h.play(Duration::from_millis(400)).await;
    assert_eq!(h.prepared(), None, "1.6 s remain");
    h.play(Duration::from_millis(200)).await;
    assert_eq!(h.prepared().as_deref(), Some("b"), "1.4 s remain");
    h.next_downloaded().await;
    let batches = h.play(Duration::from_millis(880)).await;
    assert!(batches.is_empty(), "{batches:?}");
    // half a second before a ends, b starts over it: reported as a switch without a crossfade
    let batches = h.play(Duration::from_millis(60)).await;
    assert_eq!(
        batches,
        [strings(["end a finished", "advanced b", "start b"])]
    );
    let batches = h.play(WHOLE_TONE).await;
    assert_eq!(batches, [strings(["end b finished", "queueEnd"])]);
}

#[tokio::test(flavor = "multi_thread")]
async fn repeating_with_a_crossfade_reports_what_repeating_without_one_does() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    h.player
        .configure_queue(Some(Repeat::Track), Some(Millis(500)))
        .await
        .unwrap();
    let tracks = h.tones(&["a", "b"]).await;
    h.add(tracks, None).await;
    h.events_until("start").await;
    h.play(Duration::from_millis(600)).await;
    h.next_downloaded().await;
    let batches = h.play(Duration::from_millis(1000)).await;
    assert_eq!(batches, [strings(["end a finished", "start a"])]);
    assert_eq!(h.queue(), ["b"]);

    h.player
        .configure_queue(Some(Repeat::Queue), None)
        .await
        .unwrap();
    let batches = h.play(Duration::from_millis(500)).await;
    assert_eq!(batches, [strings(["repeat queue"])]);
    assert_eq!(h.prepared().as_deref(), Some("b"));
    h.next_downloaded().await;
    let batches = h.play(Duration::from_millis(1000)).await;
    assert_eq!(
        batches,
        [strings([
            "end a finished",
            "added a",
            "advanced b",
            "start b"
        ])]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn crossfades_over_twelve_seconds_are_refused() {
    let h = Harness::new(Duration::from_secs(1)).await;
    let refused = h
        .player
        .configure_queue(Some(Repeat::Queue), Some(Millis(12_001)))
        .await
        .unwrap_err();
    assert_eq!(refused.code, ErrorCode::BadRequest);
    let queue = h.player.queue_state();
    assert_eq!(
        (queue.repeat, queue.crossfade),
        (Repeat::Off, Millis(0)),
        "a refused change changes nothing"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn repeating_a_track_and_turning_it_off() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    let tracks = h.tones(&["a", "b"]).await;
    h.player
        .configure_queue(Some(Repeat::Track), None)
        .await
        .unwrap();
    h.add(tracks, None).await;
    assert_eq!(
        h.events_until("start").await,
        strings(["repeat track", "added a b", "advanced a", "start a"])
    );

    let batches = h.play(WHOLE_TONE).await;
    assert_eq!(batches, [strings(["end a finished", "start a"])]);
    assert_eq!(h.queue(), ["b"], "the queue waits");

    h.play(Duration::from_millis(1200)).await;
    assert_eq!(h.prepared().as_deref(), Some("a"));
    h.player
        .configure_queue(Some(Repeat::Off), None)
        .await
        .unwrap();
    h.tick();
    h.until("b is prepared instead", |_| {
        h.prepared().as_deref() == Some("b")
    })
    .await;
    let batches = h.play(WHOLE_TONE).await;
    assert_eq!(
        batches,
        [
            strings(["repeat off"]),
            strings(["end a finished", "advanced b", "start b"])
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn repeating_the_queue_sends_finished_tracks_to_its_end() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    let tracks = h.tones(&["a", "b"]).await;
    h.player
        .configure_queue(Some(Repeat::Queue), None)
        .await
        .unwrap();
    h.add(tracks, None).await;
    h.events_until("start").await;

    let batches = h.play(WHOLE_TONE).await;
    assert_eq!(
        batches,
        [strings([
            "end a finished",
            "added a",
            "advanced b",
            "start b"
        ])]
    );
    let batches = h.play(WHOLE_TONE).await;
    assert_eq!(
        batches,
        [strings([
            "end b finished",
            "added b",
            "advanced a",
            "start a"
        ])]
    );
    assert_eq!(h.queue(), ["b"]);

    // a queue of one track plays it again and again
    let b = h.player.state().queue.head().unwrap().id.clone();
    h.player.queue_remove(&[b]).await.unwrap();
    let batches = h.play(WHOLE_TONE).await;
    assert_eq!(
        batches,
        [
            strings(["removed b"]),
            strings(["end a finished", "added a", "advanced a", "start a"])
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn skipping_to_a_prepared_item_is_instant() {
    // a lead longer than the tones prepares the next item right away
    let mut h = Harness::new(Duration::from_secs(10)).await;
    let tracks = h.tones(&["a", "b", "c", "d", "e"]).await;
    h.add(tracks, None).await;
    h.events_until("start").await;
    h.until("b is prepared", |_| h.prepared().as_deref() == Some("b"))
        .await;

    // the skip's events arrive before it returns
    h.player.skip(None).await.unwrap();
    assert_eq!(
        h.events().await,
        strings(["end a replaced", "advanced b", "start b"])
    );

    // skipping to an item drops the ones before it; it is not prepared, so b plays on meanwhile
    let to = h.player.queue_page(2, 1).items[0].id.clone();
    h.player.skip(Some(&to)).await.unwrap();
    assert_eq!(
        h.events_until("start").await,
        strings(["removed c d", "end b replaced", "advanced e", "start e"])
    );

    h.player.skip(None).await.unwrap();
    assert_eq!(h.events().await, strings(["end e stopped", "queueEnd"]));
    assert!(h.player.info().track.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn skipping_waits_for_an_unprepared_item_and_skips_one_still_loading() {
    // with no lead, nothing is prepared while a track plays
    let mut h = Harness::new(Duration::ZERO).await;
    let tracks = h.tones(&["a", "b"]).await;
    // an item whose loading never finishes: a server that never answers
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stalled = found(
        "stalled",
        TrackRef {
            source: "http".into(),
            uri: format!("http://{}/stalled.mp3", silent.local_addr().unwrap()),
        },
    );
    let mut tracks = tracks.into_iter();
    h.add(tracks.next().into_iter().collect(), None).await;
    h.events_until("start").await;
    h.add(vec![stalled], None).await;
    h.add(tracks.collect(), None).await;
    assert_eq!(h.prepared(), None);

    h.player.skip(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.events().await, strings(["added stalled", "added b"]));
    assert_eq!(h.player.info().track.unwrap().info.title, "a", "a plays on");

    // the stalled item is skipped as well
    h.player.skip(None).await.unwrap();
    assert_eq!(
        h.events_until("start").await,
        strings(["removed stalled", "end a replaced", "advanced b", "start b"])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn items_that_fail_to_load_are_removed_until_too_many_fail() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    let mut tracks = vec![missing("m1")];
    tracks.extend(h.tones(&["a"]).await);
    h.add(tracks, None).await;
    assert_eq!(
        h.events_until("start").await,
        strings([
            "added m1 a",
            "error m1",
            "removed m1",
            "advanced a",
            "start a"
        ])
    );
    h.player.skip(None).await.unwrap();
    assert_eq!(h.events().await, strings(["end a stopped", "queueEnd"]));

    let mut tracks: Vec<Track> = (2..=6).map(|i| missing(&format!("m{i}"))).collect();
    tracks.extend(h.tones(&["b"]).await);
    h.add(tracks, None).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut expected = strings(["added m2 m3 m4 m5 m6 b"]);
    for i in 2..=6 {
        expected.extend([format!("error m{i}"), format!("removed m{i}")]);
    }
    assert_eq!(h.events().await, expected, "the queue gives up after five");
    assert_eq!(h.queue(), ["b"]);

    // any change by the client lets it try again
    h.player
        .configure_queue(Some(Repeat::Queue), None)
        .await
        .unwrap();
    assert_eq!(
        h.events_until("start").await,
        strings(["repeat queue", "advanced b", "start b"])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_prepared_item_follows_the_head_of_the_queue() {
    let mut h = Harness::new(Duration::from_secs(10)).await;
    let tracks = h.tones(&["a", "b", "c"]).await;
    let mut tracks = tracks.into_iter();
    h.add(vec![tracks.next().unwrap()], None).await;
    h.events_until("start").await;
    h.add(vec![tracks.next().unwrap()], None).await;
    h.until("b is prepared", |_| h.prepared().as_deref() == Some("b"))
        .await;

    h.add(vec![tracks.next().unwrap()], Some(0)).await;
    h.until("c is prepared", |_| h.prepared().as_deref() == Some("c"))
        .await;
    let next = h.player.deck().next().map(|t| t.id);
    assert_eq!(next, h.player.state().prepared.as_ref().map(|(id, _)| *id));

    let c = h.player.state().queue.head().unwrap().id.clone();
    h.player.queue_remove(&[c]).await.unwrap();
    h.until("b is prepared again", |_| {
        h.prepared().as_deref() == Some("b")
    })
    .await;

    // a track's fuller metadata reaches the queue once it is loaded
    let d = h.tone_file("d").await;
    let placeholder = found(
        "placeholder",
        TrackRef {
            source: "local".into(),
            uri: d,
        },
    );
    h.add(vec![placeholder], Some(0)).await;
    h.until("d is prepared", |_| h.prepared().as_deref() == Some("d"))
        .await;
    assert_eq!(
        h.events().await,
        strings([
            "added b",
            "added c",
            "removed c",
            "added placeholder",
            "updated d"
        ])
    );
}

mod layers;
