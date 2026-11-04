//! Queued queries, which the player loads itself in the background. They are links to tones on
//! the harness's media server, which can hold them back to decide which load finishes when.

use super::*;
use crate::node::player::loading::Found;
use crate::sources::PlaylistInfo;

#[tokio::test(flavor = "multi_thread")]
async fn queries_load_in_place_whichever_finishes_first() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    h.media.hold("a");
    let ids = h.add_links(&["a", "b", "c"], None).await;
    let page = h.player.queue_page(0, 10);
    assert!(
        page.items.iter().all(|item| item.is_loading()),
        "queued at once, loading"
    );

    // b and c load meanwhile, but nothing plays before a, the first
    let mut events = h.events_until("loaded").await;
    if events.len() < 3 {
        events.extend(h.events_until("loaded").await);
    }
    events.sort();
    assert_eq!(
        events,
        strings(["added a b c", "loaded b as b", "loaded c as c"])
    );
    assert_eq!(h.queue(), strings(["a", "b", "c"]));
    assert!(h.player.state().queue.head().unwrap().is_loading());
    assert!(h.player.deck().is_idle());

    h.media.release("a");
    assert_eq!(
        h.events_until("start").await,
        strings(["loaded a as a", "advanced a", "start a"])
    );
    assert_eq!(h.queue(), strings(["b", "c"]));
    assert_eq!(h.player.info().item_id.as_ref(), Some(&ids[0]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_query_that_fails_to_load_is_reported_and_removed() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    h.add_links(&["missing", "b"], None).await;
    let mut events = h.events_until("start").await;
    // b may load before `missing` fails, but it plays only once it is first
    let error = events
        .iter()
        .position(|e| e == "error missing")
        .expect("the failure is reported");
    assert_eq!(events[error + 1], "removed missing");
    events.drain(error..error + 2);
    assert_eq!(
        events,
        strings(["added missing b", "loaded b as b", "advanced b", "start b"])
    );
    assert!(h.player.state().loads.is_empty());

    h.play(WHOLE_TONE).await;
    assert_eq!(h.events().await, Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_skip_to_a_loading_item_waits_for_it() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    let tracks = h.tones(&["x"]).await;
    h.add(tracks, None).await;
    h.events_until("start").await;
    h.media.hold("b");
    let ids = h.add_links(&["a", "b"], None).await;
    h.events_until("loaded").await;

    h.player.skip(Some(&ids[1])).await.unwrap();
    assert_eq!(
        h.events().await,
        strings(["removed a"]),
        "x plays on until b has loaded"
    );
    h.play(Duration::from_millis(200)).await;
    assert_eq!(h.player.info().track.unwrap().info.title, "x");

    h.media.release("b");
    assert_eq!(
        h.events_until("start").await,
        strings(["loaded b as b", "end x replaced", "advanced b", "start b"])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn items_skipped_or_removed_stop_loading() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    let names = ["a", "b", "c", "d", "e"];
    for name in names {
        h.media.hold(name);
    }
    let ids = h.add_links(&names, None).await;
    h.until("three load", |p| p.state().loads.len() == 3).await;
    assert!(
        ["a", "b", "c"]
            .iter()
            .zip(&ids)
            .all(|(_, id)| h.player.state().loads.contains_key(id))
    );

    // skipping to e drops a to d, and e, now first, loads at once
    h.player.skip(Some(&ids[4])).await.unwrap();
    {
        let state = h.player.state();
        assert_eq!(state.loads.keys().collect::<Vec<_>>(), [&ids[4]]);
        assert!(state.loads[&ids[4]].promoted());
    }
    h.player.queue_remove(&[ids[4].clone()]).await.unwrap();
    assert!(h.player.state().loads.is_empty());
    for name in names {
        h.media.release(name);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let events = h.events().await;
    assert!(
        !events.iter().any(|e| e.starts_with("loaded")),
        "{events:?}"
    );
    assert!(h.player.deck().is_idle());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_query_added_on_its_own_loads_at_once() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    let names = ["a", "b", "c", "d", "e"];
    for name in names {
        h.media.hold(name);
    }
    h.add_links(&names, None).await;
    h.until("three load", |p| p.state().loads.len() == 3).await;

    // a client waits for it, as for a `/play`: it loads ahead of d and e, with a normal turn
    let x = h.add_links(&["x"], None).await;
    assert!(h.player.state().loads[&x[0]].promoted());
    assert_eq!(
        h.events_until("loaded").await,
        strings(["added a b c d e", "added x", "loaded x as x"])
    );
    assert_eq!(h.queue(), strings(["a", "b", "c", "d", "e", "x"]));
    assert_eq!(h.player.state().loads.len(), 3, "a to c still load");
    for name in names {
        h.media.release(name);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_loading_queue_has_not_ended() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    let tracks = h.tones(&["x"]).await;
    h.add(tracks, None).await;
    h.events_until("start").await;
    h.media.hold("a");
    h.add_links(&["a"], None).await;
    // x ends while a is still loading: the queue waits for it
    let batches = h.play(WHOLE_TONE).await;
    assert_eq!(batches, [strings(["added a"]), strings(["end x finished"])]);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(h.events().await, Vec::<String>::new());

    h.media.release("a");
    assert_eq!(
        h.events_until("start").await,
        strings(["loaded a as a", "advanced a", "start a"])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_playlist_loads_in_place() {
    let mut h = Harness::new(Duration::from_secs(1)).await;
    // the list's own load never finishes: the test hands the player what it found
    h.media.hold("list");
    let ids = h.add_links(&["list", "after"], None).await;
    h.events_until("loaded after").await;
    let tracks = h.tones(&["one", "two"]).await;
    let list = h.player.state().queue.get(&ids[0]).cloned().unwrap();
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
    {
        let node = Arc::clone(&h.node);
        let mut state = h.player.state();
        h.player.loaded(&mut state, &node, &list, found);
        h.player.plan_next(&mut state);
    }
    assert_eq!(
        h.events_until("start").await,
        strings(["loaded list as list two", "advanced list", "start list"])
    );

    let state = h.player.state();
    let titles: Vec<_> = state
        .queue
        .iter()
        .map(|item| item.track.info.title.clone())
        .collect();
    assert_eq!(titles, ["two", "after"]);
    assert!(
        state
            .items
            .values()
            .any(|item| item.id == ids[0] && item.track.info.title == "one"),
        "the first track keeps the item's id"
    );
    assert!(state.loads.is_empty(), "the list's own load was cancelled");
}
