//! The deck: tracks back to back without gaps, events, volume, stalls.

use std::sync::Arc;
use std::time::Duration;

use entroximer_core::deck::{Deck, DeckEvent, DeckTrack};
use entroximer_core::dsp::{GraphSpec, NodeRegistry};
use entroximer_core::player::{Player, PlayerConfig};
use entroximer_core::producer::Producer;
use entroximer_core::producer::ffmpeg::FfmpegProducer;
use entroximer_core::producer::process::{ProcessProducer, ProcessSpec};
use entroximer_core::producer::{ProducerCaps, TimestampMode};
use entroximer_core::timeline::Ticks;

mod common;
use common::{BLOCK_FRAMES, assert_playing_second, at, expected_hz, fixture, frequency};

struct Bench {
    deck: Deck,
    rt: tokio::runtime::Runtime,
}

impl Bench {
    fn new() -> Self {
        Self::stuck_after(Duration::from_secs(10))
    }

    fn stuck_after(threshold: Duration) -> Self {
        Self {
            deck: Deck::new(threshold),
            rt: tokio::runtime::Runtime::new().unwrap(),
        }
    }

    fn track(&self, id: u64, producer: impl Producer, start: Option<Ticks>) -> DeckTrack {
        DeckTrack {
            id,
            player: Player::new(
                self.rt.handle().clone(),
                Arc::new(producer),
                PlayerConfig::default(),
                start,
            ),
            end_at: None,
        }
    }

    fn tone(&self, id: u64, start: Option<Ticks>) -> DeckTrack {
        self.track(
            id,
            FfmpegProducer::new("ffmpeg", fixture().to_string_lossy()),
            start,
        )
    }

    /// Renders until `seconds` of audio were produced while a track was playing.
    fn render(&mut self, seconds: f64) -> Vec<f32> {
        let frames = (seconds * 48_000.0).round() as usize;
        let mut out = Vec::with_capacity(frames * 2);
        let mut block = vec![0.0; BLOCK_FRAMES * 2];
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while out.len() < frames * 2 {
            let want = (frames - out.len() / 2).min(BLOCK_FRAMES);
            let before = self.deck.current().map(|t| t.player.position());
            self.deck.render(&mut block[..want * 2]);
            let advanced = before != self.deck.current().map(|t| t.player.position());
            // skip blocks where the current track was still loading (pure silence)
            if advanced || self.deck.is_idle() {
                out.extend_from_slice(&block[..want * 2]);
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(std::time::Instant::now() < deadline, "rendering stalled");
        }
        out
    }
}

fn ids(events: &[DeckEvent]) -> Vec<String> {
    events
        .iter()
        .map(|e| match e {
            DeckEvent::Started(id) => format!("start {id}"),
            DeckEvent::Ended(id, reason) => format!("end {id} {reason:?}"),
            DeckEvent::Failed(id, _) => format!("fail {id}"),
            DeckEvent::Stuck(id, _) => format!("stuck {id}"),
        })
        .collect()
}

#[test]
fn next_track_follows_without_a_gap() {
    let mut bench = Bench::new();
    let mut first = bench.tone(1, None);
    first.end_at = Some(at(1.0));
    let second = bench.tone(2, Some(at(30.0)));
    bench.deck.play(first);
    bench.deck.set_next(Some(second));

    // the point is that a preloaded track follows without a gap, so wait for its download
    // (a fixed sleep is not enough on a busy machine)
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let preloaded = |deck: &Deck| {
        deck.next().is_some_and(|next| {
            next.player
                .buffered()
                .iter()
                .any(|&(start, end)| start <= at(30.0) && end >= at(30.5))
        })
    };
    while !preloaded(&bench.deck) {
        assert!(
            std::time::Instant::now() < deadline,
            "the next track never downloaded"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let audio = bench.render(1.5);

    assert_eq!(
        ids(&bench.deck.drain_events()),
        ["start 1", "end 1 Finished", "start 2"]
    );
    // 0.2 s windows on both sides of the switch: the end of the first track, the start of the
    // second (which begins at 30 s)
    let switch = at(1.0) as usize * 2;
    let window = at(0.2) as usize * 2;
    assert_playing_second(&audio[switch - window..switch], 0.5);
    assert_playing_second(&audio[switch..switch + window], 30.0);
    // the tone crosses zero but never stays near it; a gap would be a run of silent frames
    let longest_quiet = audio[switch - 4800..switch + 4800]
        .chunks(2)
        .map(|f| f[0].abs() < 1e-4)
        .fold((0, 0), |(run, max), quiet| {
            let run = if quiet { run + 1 } else { 0 };
            (run, max.max(run))
        })
        .1;
    assert!(
        longest_quiet < 10,
        "{longest_quiet} silent frames at the switch"
    );
}

#[test]
fn replace_stop_and_volume() {
    let mut bench = Bench::new();
    bench.deck.play(bench.tone(1, None));
    let full = bench.render(0.2);
    bench.deck.set_volume(0.5);
    let half = bench.render(0.2);
    bench.deck.play(bench.tone(2, None));
    bench.deck.stop();
    assert_eq!(
        ids(&bench.deck.drain_events()),
        ["start 1", "end 1 Replaced", "start 2", "end 2 Stopped"]
    );
    assert!(bench.deck.is_idle());

    let peak = |audio: &[f32]| audio.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    let ratio = peak(&half) / peak(&full);
    assert!(
        (0.47..=0.53).contains(&ratio),
        "volume 0.5 scaled the peak by {ratio}"
    );

    let mut silence = vec![1.0; 1920];
    bench.deck.render(&mut silence);
    assert!(
        silence.iter().all(|&s| s == 0.0),
        "an idle deck outputs silence"
    );
}

#[test]
fn a_stalled_track_is_reported_once() {
    let mut bench = Bench::stuck_after(Duration::from_millis(100));
    let stalled = ProcessProducer::new(ProducerCaps::default(), None, |_| {
        (ProcessSpec::new("sleep").arg("30"), TimestampMode::Absolute)
    });
    bench.deck.play(bench.track(7, stalled, None));
    let mut block = vec![0.0; BLOCK_FRAMES * 2];
    for _ in 0..30 {
        bench.deck.render(&mut block);
        std::thread::sleep(Duration::from_millis(10));
    }
    let events = ids(&bench.deck.drain_events());
    assert_eq!(events, ["start 7", "stuck 7"]);
}

#[test]
fn ending_without_a_next_track_leaves_the_deck_idle() {
    let mut bench = Bench::new();
    let mut track = bench.tone(1, None);
    track.end_at = Some(at(0.3));
    bench.deck.play(track);
    bench.render(0.3);
    let mut block = vec![0.0; BLOCK_FRAMES * 2];
    bench.deck.render(&mut block);
    assert!(bench.deck.is_idle());
    assert!(bench.deck.current().is_none());
    assert_eq!(
        ids(&bench.deck.drain_events()),
        ["start 1", "end 1 Finished"]
    );
}

#[test]
fn a_rate_plays_the_track_faster_and_higher() {
    let mut bench = Bench::new();
    let graph = GraphSpec {
        rate: Some(1.25),
        ..GraphSpec::default()
    };
    let effects = NodeRegistry::with_builtins()
        .compile(&graph, 48_000.0)
        .unwrap();
    bench.deck.set_effects(Some(effects));
    bench.deck.play(bench.tone(1, Some(at(10.0))));

    let audio = bench.render(2.0);
    let position = bench.deck.current().unwrap().player.position();
    // two seconds heard took two and a half of the track
    assert!(
        (position - at(12.5)).abs() < at(0.1),
        "position {:.2} s",
        position as f64 / 48_000.0
    );
    // a second into the output the track is a second and a quarter in, and its tone 1.25 times
    // as high
    let window = &audio[at(1.0) as usize * 2..at(1.1) as usize * 2];
    let heard = frequency(window);
    let expected = expected_hz(11.3) * 1.25;
    assert!(
        (heard - expected).abs() < 6.0,
        "expected {expected} Hz, heard {heard:.1} Hz"
    );
}
