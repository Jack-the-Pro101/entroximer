//! Clips on layers besides the music: they play over the queue without touching it, wait their
//! turn, fail on their own, and end once each.

use entroximer_core::producer::process::{ProcessProducer, ProcessSpec};
use entroximer_core::producer::{ProducerCaps, TimestampMode};
use serde_json::json;

use super::*;
use crate::api::protocol::{ClipInfo, ClipStatus, PlayClip, StopClips};

impl Harness {
    /// Plays the tone `name` as clip `id` on `layer`.
    async fn clip(&self, layer: &str, id: &str, name: &str) -> ClipInfo {
        let query = self.tone_file(name).await;
        let clip = serde_json::from_value::<PlayClip>(json!({
            "guildId": "2",
            "layer": layer,
            "clipId": id,
            "track": { "query": query },
        }))
        .unwrap();
        self.player.clip_play(&self.node, clip, None).await.unwrap()
    }

    fn output_playing(&self) -> Option<bool> {
        *self.player.output_playing.lock().unwrap()
    }

    fn clips(&self, layer: &str) -> Vec<(String, ClipStatus)> {
        let info = self.player.info();
        let layer = info.layers.into_iter().find(|l| l.name == layer).unwrap();
        layer
            .clips
            .into_iter()
            .map(|clip| (clip.clip_id, clip.status))
            .collect()
    }

    fn music_position(&self) -> Ticks {
        self.player.deck().current().unwrap().player.position()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_clip_plays_over_the_queue_and_leaves_it_alone() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    let tracks = h.tones(&["a", "b"]).await;
    h.add(tracks, None).await;
    h.events_until("start").await;
    h.play(Duration::from_millis(500)).await;

    let clip = h.clip("sfx", "c1", "beep").await;
    assert_eq!(clip.status, ClipStatus::Playing);
    // its start came before the response
    assert_eq!(h.events().await, strings(["clip start c1"]));
    let info = h.player.info();
    assert_eq!(info.track.unwrap().info.title, "a");
    assert_eq!(h.clips("sfx"), [("c1".to_owned(), ClipStatus::Playing)]);

    // a ends 1.5 s from now and the queue goes on; the clip ends half a second later
    let batches = h.play(Duration::from_millis(2100)).await;
    assert_eq!(
        batches,
        [
            strings(["end a finished", "advanced b", "start b"]),
            strings(["clip end c1 finished"])
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pausing_the_music_leaves_clips_playing_with_the_output_on() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    let tracks = h.tones(&["a"]).await;
    h.add(tracks, None).await;
    h.events_until("start").await;
    h.play(Duration::from_millis(500)).await;
    let pause = serde_json::from_value(json!({ "guildId": "2", "paused": true })).unwrap();
    h.player.update(&h.node, pause).await.unwrap();
    assert_eq!(h.output_playing(), Some(false));
    let held = h.music_position();

    h.clip("sfx", "c1", "beep").await;
    assert_eq!(h.output_playing(), Some(true), "the clip is heard");
    let batches = h.play(WHOLE_TONE).await;
    assert_eq!(
        batches,
        [
            strings(["clip start c1"]),
            strings(["clip end c1 finished"])
        ]
    );
    assert_eq!(
        h.output_playing(),
        Some(false),
        "then there is nothing to hear"
    );
    assert_eq!(h.music_position(), held, "the music held its position");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_queue_layer_opens_only_the_clip_after_the_playing_one() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    for id in ["c1", "c2", "c3"] {
        h.clip("voice", id, id).await;
    }
    assert_eq!(
        h.clips("voice"),
        [
            ("c1".to_owned(), ClipStatus::Playing),
            ("c2".to_owned(), ClipStatus::Waiting),
            ("c3".to_owned(), ClipStatus::Waiting),
        ]
    );
    assert_eq!(h.player.mixer().players().count(), 2, "c3 waits unopened");
    assert_eq!(h.events().await, strings(["clip start c1"]));

    let batches = h.play(WHOLE_TONE).await;
    assert_eq!(
        batches,
        [strings(["clip end c1 finished", "clip start c2"])]
    );
    assert_eq!(h.player.mixer().players().count(), 2, "c3 is next now");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_clip_fails_so_that_the_ones_after_it_play() {
    let mut h = Harness::build(Duration::from_secs(1), Duration::from_millis(100)).await;
    let stalled = Loaded {
        track: h.tones(&["stalled"]).await.remove(0),
        producer: Arc::new(ProcessProducer::new(ProducerCaps::default(), None, |_| {
            (ProcessSpec::new("sleep").arg("30"), TimestampMode::Absolute)
        })),
    };
    h.player
        .play_loaded(&h.node, "voice", "c1", stalled)
        .await
        .unwrap();
    h.clip("voice", "c2", "c2").await;
    let mut events = h.events().await;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !events.iter().any(|e| e == "clip start c2") {
        assert!(Instant::now() < deadline, "only {events:?}");
        h.output.read_exact(&mut h.buffer).unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        events.extend(h.events().await);
    }
    assert_eq!(
        events,
        strings([
            "clip start c1",
            "clip stuck c1",
            "clip end c1 failed",
            "clip start c2"
        ])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_clips_ends_the_waiting_ones_without_a_start() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    for id in ["c1", "c2", "c3", "c4"] {
        h.clip("voice", id, id).await;
    }
    h.events().await;
    let stop = |clip_id: Option<&str>| {
        serde_json::from_value::<StopClips>(json!({
            "guildId": "2",
            "layer": "voice",
            "clipId": clip_id,
        }))
        .unwrap()
    };

    // the next one, which has a player, and one waiting without
    h.player.clip_stop(stop(Some("c2"))).await.unwrap();
    h.player.clip_stop(stop(Some("c4"))).await.unwrap();
    assert_eq!(
        h.events().await,
        strings(["clip end c2 stopped", "clip end c4 stopped"])
    );
    assert_eq!(
        h.clips("voice"),
        [
            ("c1".to_owned(), ClipStatus::Playing),
            ("c3".to_owned(), ClipStatus::Waiting),
        ]
    );
    let missing = h.player.clip_stop(stop(Some("c2"))).await.unwrap_err();
    assert_eq!(missing.code, ErrorCode::NotFound);

    let layer = h.player.clip_stop(stop(None)).await.unwrap();
    assert!(layer.clips.is_empty());
    let mut events = h.events().await;
    events.sort();
    assert_eq!(
        events,
        strings(["clip end c1 stopped", "clip end c3 stopped"])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn destroying_ends_every_clip_once_as_cleaned_up() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    for id in ["c1", "c2", "c3"] {
        h.clip("voice", id, id).await;
    }
    h.clip("sfx", "s1", "s1").await;
    h.events().await;
    h.player.destroy().await;
    let mut events = h.events().await;
    events.sort();
    assert_eq!(
        events,
        strings([
            "clip end c1 cleanup",
            "clip end c2 cleanup",
            "clip end c3 cleanup",
            "clip end s1 cleanup"
        ])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn removing_a_layer_stops_its_clips_and_its_ducking() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    h.clip("sfx", "s1", "s1").await;
    let duck = serde_json::from_value(json!({
        "guildId": "2",
        "layer": "voice",
        "duck": { "layers": ["main", "sfx"], "volume": 25 },
    }))
    .unwrap();
    h.player.layer_update(&h.node, duck).await.unwrap();
    h.events().await;

    h.player.layer_remove("sfx").await.unwrap();
    assert_eq!(h.events().await, strings(["clip end s1 stopped"]));
    let info = h.player.info();
    assert_eq!(info.layers.len(), 1);
    assert_eq!(info.layers[0].duck.as_ref().unwrap().layers, ["main"]);
    let gone = h.player.layer_remove("sfx").await.unwrap_err();
    assert_eq!(gone.code, ErrorCode::NotFound);
}

#[tokio::test(flavor = "multi_thread")]
async fn clips_wait_with_the_music_while_the_client_is_away() {
    let h = Harness::new(Duration::from_secs(1)).await;
    h.clip("sfx", "c1", "beep").await;
    assert_eq!(h.output_playing(), Some(true));
    h.player.set_client_present(false);
    assert_eq!(h.output_playing(), Some(false));
    h.player.set_client_present(true);
    assert_eq!(h.output_playing(), Some(true));
}
