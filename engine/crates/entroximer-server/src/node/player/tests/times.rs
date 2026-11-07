//! Queue items that play part of their track, from its `startTime` to its `endTime`.

use super::*;
use crate::api::protocol::Times;
use crate::node::player::loading::Found;
use crate::sources::PlaylistInfo;

const HALF: Millis = Millis(500);
const ONE_AND_A_HALF: Millis = Millis(1500);

/// The current track's position.
fn position(h: &Harness) -> Millis {
    h.player.player_state().position
}

#[tokio::test(flavor = "multi_thread")]
async fn an_item_plays_from_its_start_time_to_its_end_time() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    let mut tracks = h.tones(&["a", "b"]).await;
    tracks[0].start_time = Some(HALF);
    tracks[0].end_time = Some(ONE_AND_A_HALF);
    h.add(tracks, None).await;
    assert_eq!(
        h.events_until("start").await,
        strings(["added a b", "advanced a", "start a"])
    );
    let playing = h.player.info().track.unwrap();
    assert_eq!(
        (playing.start_time, playing.end_time),
        (Some(HALF), Some(ONE_AND_A_HALF))
    );
    assert!((HALF..Millis(600)).contains(&position(&h)));

    // a second of it plays; b follows as it would after a's end
    let batches = h.play(Duration::from_millis(900)).await;
    assert!(batches.is_empty(), "{batches:?}");
    assert_eq!(h.prepared().as_deref(), Some("b"));
    let batches = h.play(Duration::from_millis(200)).await;
    assert_eq!(
        batches,
        [strings(["end a finished", "advanced b", "start b"])]
    );
    assert!(position(&h) < Millis(200), "b plays from its beginning");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repeated_item_plays_again_from_its_start_time() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    h.player
        .configure_queue(Some(Repeat::Track), None)
        .await
        .unwrap();
    let mut tracks = h.tones(&["a"]).await;
    tracks[0].start_time = Some(Millis(1000));
    h.add(tracks, None).await;
    h.events_until("start").await;

    let batches = h.play(Duration::from_millis(1100)).await;
    assert_eq!(batches, [strings(["end a finished", "start a"])]);
    assert!(
        (Millis(1000)..Millis(1200)).contains(&position(&h)),
        "at {:?}",
        position(&h)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repeated_queue_keeps_each_items_times() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    h.player
        .configure_queue(Some(Repeat::Queue), None)
        .await
        .unwrap();
    let mut tracks = h.linked_tones(&["a", "b"]).await;
    tracks[0].end_time = Some(HALF);
    h.add(tracks, None).await;
    h.events_until("start").await;
    h.play(Duration::from_millis(600)).await;
    h.play(WHOLE_TONE).await;

    // a comes round again, from the loop cache, still ending at its end time
    let current = {
        let state = h.player.state();
        state.current.and_then(|id| state.items.get(&id)).cloned()
    };
    let current = current.unwrap();
    assert_eq!(h.names[&current.id], "a");
    assert_eq!(current.track.end_time, Some(HALF));
    let batches = h.play(Duration::from_millis(600)).await;
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
async fn a_repeated_queue_plays_the_part_it_kept_whatever_its_times() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    h.player
        .configure_queue(Some(Repeat::Queue), None)
        .await
        .unwrap();
    let mut tracks = h.linked_tones(&["a", "b"]).await;
    tracks[0].start_time = Some(Millis(1000));
    tracks[0].end_time = Some(ONE_AND_A_HALF);
    let encoded = tracks[0].encoded.clone();
    h.add(tracks, None).await;
    h.events_until("start").await;
    h.play(Duration::from_millis(600)).await;

    // a downloaded from shortly before its start time, and is kept from there
    assert_eq!(h.player.state().kept.len(), 1);
    let kept = h.node.loop_cache.get(&encoded, Times::default());
    assert!(kept.is_none(), "its beginning is not on disk");
    // a later start is
    let a = id_of(&h, "a");
    h.player
        .update_item(&times_of(&a, serde_json::json!({ "startTime": 1200 })))
        .await
        .unwrap();
    h.until("b downloads", |p| {
        p.deck()
            .current()
            .is_some_and(|current| current.player.media_end().is_some())
    })
    .await;
    // loading them again would fail now
    for name in ["a", "b"] {
        std::fs::remove_file(h.dir.path().join(format!("{name}.webm"))).unwrap();
    }

    let batches = h.play(WHOLE_TONE).await;
    assert_eq!(
        batches,
        [
            strings(["updated a"]),
            strings(["end b finished", "added b", "advanced a", "start a"])
        ]
    );
    assert!(
        (Millis(1200)..Millis(1400)).contains(&position(&h)),
        "at {:?}",
        position(&h)
    );
    let batches = h.play(Duration::from_millis(400)).await;
    assert_eq!(
        batches,
        [strings([
            "end a finished",
            "added a",
            "advanced b",
            "start b"
        ])]
    );
    assert_eq!(h.player.state().kept.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_query_loads_into_a_track_with_its_times() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    h.tone_file("a").await;
    let selector = TrackSelector::Query {
        query: h.media.link("a"),
        start_index: None,
        user_data: None,
        start_time: Some(HALF),
        end_time: None,
    };
    let item = h.node.queue_item(&selector).unwrap();
    assert_eq!(item.track.start_time, Some(HALF), "its stand-in shows them");
    h.names.insert(item.id.clone(), "a".into());
    h.player.queue_add(vec![item], None).await.unwrap();
    assert_eq!(
        h.events_until("start").await,
        strings(["added a", "loaded a as a", "advanced a", "start a"])
    );
    assert_eq!(h.player.info().track.unwrap().start_time, Some(HALF));
    assert!((HALF..Millis(600)).contains(&position(&h)));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_query_whose_track_is_shorter_than_its_start_time_is_reported_and_removed() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    h.tone_file("a").await;
    let selector = TrackSelector::Query {
        query: h.media.link("a"),
        start_index: None,
        user_data: None,
        start_time: Some(Millis(5000)),
        end_time: None,
    };
    let item = h.node.queue_item(&selector).unwrap();
    h.names.insert(item.id.clone(), "a".into());
    h.player.queue_add(vec![item], None).await.unwrap();
    assert_eq!(
        h.events_until("queueEnd").await,
        strings(["added a", "error a", "removed a", "queueEnd"])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn times_the_track_cannot_play_are_refused() {
    let h = Harness::new(Duration::from_secs(1)).await;
    let track = h.tones(&["a"]).await.remove(0);
    let backwards = TrackSelector::Encoded {
        encoded: track.encoded.clone(),
        user_data: None,
        start_time: Some(ONE_AND_A_HALF),
        end_time: Some(HALF),
    };
    let error = h.node.queue_item(&backwards).unwrap_err();
    assert_eq!(error.code, ErrorCode::BadRequest);
    assert!(
        error.message.contains("before endTime"),
        "{}",
        error.message
    );

    // the tone is just over 2 s long, as its track says
    let too_late = TrackSelector::Encoded {
        encoded: track.encoded,
        user_data: None,
        start_time: Some(Millis(3000)),
        end_time: None,
    };
    let error = h.node.queue_item(&too_late).unwrap_err();
    assert!(
        error.message.contains("cannot start at 0:03"),
        "{}",
        error.message
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_playlists_first_track_takes_the_times() {
    let h = Harness::new(Duration::from_secs(1)).await;
    let tracks = h.tones(&["one", "two"]).await;
    let found = Found {
        tracks,
        playlist: Some(PlaylistInfo {
            name: "Tones".into(),
            author: None,
            uri: None,
            artwork_url: None,
            selected_track: None,
            total_tracks: Some(2),
        }),
        ..Found::default()
    };
    let times = Times {
        start: Some(HALF),
        end: None,
    };
    let found = found.with_times(times).unwrap();
    let starts: Vec<_> = found.tracks.iter().map(|t| t.start_time).collect();
    assert_eq!(starts, [Some(HALF), None]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_track_played_now_takes_the_updates_position_and_end() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    let track = h.tones(&["a"]).await.remove(0);
    let update: PlayerUpdate = serde_json::from_value(serde_json::json!({
        "guildId": "2",
        "track": { "encoded": track.encoded, "startTime": 100, "endTime": 1900 },
        "position": 500,
    }))
    .unwrap();
    let info = h.player.update(&h.node, update).await.unwrap();
    h.names.insert(info.item_id.unwrap(), "a".into());
    let playing = info.track.unwrap();
    assert_eq!(
        (playing.start_time, playing.end_time),
        (Some(HALF), Some(Millis(1900))),
        "the position takes the place of its startTime"
    );
    h.events_until("start").await;

    // a later end time is the track's too
    let update: PlayerUpdate =
        serde_json::from_value(serde_json::json!({ "guildId": "2", "endTime": ONE_AND_A_HALF.0 }))
            .unwrap();
    let info = h.player.update(&h.node, update).await.unwrap();
    assert_eq!(info.track.unwrap().end_time, Some(ONE_AND_A_HALF));
    let update: PlayerUpdate =
        serde_json::from_value(serde_json::json!({ "guildId": "2", "endTime": 100 })).unwrap();
    assert!(
        h.player.update(&h.node, update).await.is_err(),
        "before its start"
    );

    let batches = h.play(Duration::from_millis(1100)).await;
    assert_eq!(batches, [strings(["end a finished", "queueEnd"])]);
}

fn times_of(id: &str, times: serde_json::Value) -> crate::api::protocol::UpdateItem {
    let mut params = serde_json::json!({ "guildId": "2", "id": id });
    params
        .as_object_mut()
        .unwrap()
        .extend(times.as_object().unwrap().clone());
    serde_json::from_value(params).unwrap()
}

/// The item id of the track called `name`.
fn id_of(h: &Harness, name: &str) -> String {
    h.names
        .iter()
        .find(|(_, n)| *n == name)
        .map(|(id, _)| id.clone())
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_queued_items_times_change_even_once_it_is_prepared() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    let tracks = h.tones(&["a", "b"]).await;
    h.add(tracks, None).await;
    h.events_until("start").await;
    h.play(Duration::from_millis(1200)).await;
    assert_eq!(h.prepared().as_deref(), Some("b"));

    let b = id_of(&h, "b");
    h.player
        .update_item(&times_of(&b, serde_json::json!({ "startTime": HALF.0 })))
        .await
        .unwrap();
    assert_eq!(h.events().await, strings(["updated b"]));
    let queued = h.player.queue_page(0, 1).items.remove(0);
    assert_eq!(queued.track.start_time, Some(HALF));
    // prepared again, from its new start
    h.play(Duration::from_millis(40)).await;
    h.until("b is prepared again", |_| {
        h.prepared().as_deref() == Some("b")
    })
    .await;
    let batches = h.play(Duration::from_millis(900)).await;
    assert_eq!(
        batches,
        [strings(["end a finished", "advanced b", "start b"])]
    );
    assert!(position(&h) >= HALF, "at {:?}", position(&h));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_playing_items_new_end_applies_now_and_its_new_start_next_time() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    h.player
        .configure_queue(Some(Repeat::Track), None)
        .await
        .unwrap();
    let tracks = h.tones(&["a"]).await;
    h.add(tracks, None).await;
    h.events_until("start").await;
    // its replay is prepared, to play it from the beginning
    h.play(Duration::from_millis(1200)).await;
    assert_eq!(h.prepared().as_deref(), Some("a"));

    let a = id_of(&h, "a");
    let update = times_of(
        &a,
        serde_json::json!({ "startTime": 1000, "endTime": ONE_AND_A_HALF.0 }),
    );
    let info = h.player.update_item(&update).await.unwrap();
    let playing = info.track.unwrap();
    assert_eq!(
        (playing.start_time, playing.end_time),
        (Some(Millis(1000)), Some(ONE_AND_A_HALF))
    );
    assert!(
        (Millis(1200)..Millis(1300)).contains(&info.state.position),
        "it plays on from where it was, at {:?}",
        info.state.position
    );
    let batches = h.play(Duration::from_millis(340)).await;
    assert_eq!(batches, [strings(["end a finished", "start a"])]);
    assert!(
        (Millis(1000)..Millis(1100)).contains(&position(&h)),
        "the repeat starts at the new start, at {:?}",
        position(&h)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_end_the_playing_item_has_passed_ends_it() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    let tracks = h.tones(&["a", "b"]).await;
    h.add(tracks, None).await;
    h.events_until("start").await;
    h.play(Duration::from_millis(1000)).await;
    let a = id_of(&h, "a");
    h.player
        .update_item(&times_of(&a, serde_json::json!({ "endTime": HALF.0 })))
        .await
        .unwrap();
    let batches = h.play(Duration::from_millis(40)).await;
    assert_eq!(
        batches,
        [strings(["end a finished", "advanced b", "start b"])]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_loading_items_new_times_apply_once_it_loads() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    h.media.hold("a");
    let ids = h.add_links(&["a"], None).await;
    h.events_until("added").await;
    h.player
        .update_item(&times_of(
            &ids[0],
            serde_json::json!({ "startTime": HALF.0 }),
        ))
        .await
        .unwrap();
    // its stand-in, titled with its link
    assert_eq!(h.events().await, [format!("updated {}", h.media.link("a"))]);
    h.media.release("a");
    assert_eq!(
        h.events_until("start").await,
        strings(["loaded a as a", "advanced a", "start a"])
    );
    assert_eq!(h.player.info().track.unwrap().start_time, Some(HALF));
    assert!(position(&h) >= HALF);
}

#[tokio::test(flavor = "multi_thread")]
async fn refused_changes_to_times_change_nothing() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    let tracks = h.tones(&["a", "b"]).await;
    h.add(tracks, None).await;
    h.events_until("start").await;
    let b = id_of(&h, "b");
    h.player
        .update_item(&times_of(
            &b,
            serde_json::json!({ "endTime": ONE_AND_A_HALF.0 }),
        ))
        .await
        .unwrap();
    h.events().await;

    let backwards = times_of(&b, serde_json::json!({ "startTime": 1600 }));
    let error = h.player.update_item(&backwards).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::BadRequest);
    let too_late = times_of(&id_of(&h, "a"), serde_json::json!({ "startTime": 3000 }));
    assert_eq!(
        h.player.update_item(&too_late).await.unwrap_err().code,
        ErrorCode::BadRequest
    );
    let gone = times_of("nope", serde_json::json!({ "endTime": 5 }));
    assert_eq!(
        h.player.update_item(&gone).await.unwrap_err().code,
        ErrorCode::NotFound
    );
    assert!(h.events().await.is_empty());
    let queued = h.player.queue_page(0, 1).items.remove(0);
    assert_eq!(
        (queued.track.start_time, queued.track.end_time),
        (None, Some(ONE_AND_A_HALF))
    );
}
