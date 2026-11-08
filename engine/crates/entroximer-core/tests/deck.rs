//! The deck: tracks back to back without gaps, events, volume, stalls.

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use entroximer_core::deck::{Deck, DeckEvent, DeckTrack};
use entroximer_core::dsp::{GraphSpec, NodeRegistry};
use entroximer_core::player::{Player, PlayerConfig};
use entroximer_core::producer::Producer;
use entroximer_core::producer::ffmpeg::FfmpegProducer;
use entroximer_core::producer::process::{ProcessProducer, ProcessSpec};
use entroximer_core::producer::{ProducerCaps, ProducerStream, TimestampMode};
use entroximer_core::timeline::Ticks;
use futures_util::future::BoxFuture;

mod common;
use common::{
    BLOCK_FRAMES, assert_playing_second, at, expected_hz, fixture, frequency, tone_level,
};

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
            start: start.unwrap_or(0),
            end_at: None,
            length: None,
        }
    }

    fn tone(&self, id: u64, start: Option<Ticks>) -> DeckTrack {
        self.track(
            id,
            FfmpegProducer::new("ffmpeg", fixture().to_string_lossy()),
            start,
        )
    }

    /// Waits until the next track has downloaded a second and a half from `from`.
    fn wait_for_next(&self, from: f64) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let ready = |deck: &Deck| {
            deck.next().is_some_and(|next| {
                next.player
                    .buffered()
                    .iter()
                    .any(|&(start, end)| start <= at(from) && end >= at(from + 1.5))
            })
        };
        while !ready(&self.deck) {
            assert!(
                std::time::Instant::now() < deadline,
                "the next track never downloaded"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Plays the steps from 48 s to 51 s, with the steps from 10 s next and a crossfade.
    fn crossfading(crossfade: f64) -> Self {
        let mut bench = Self::new();
        bench.deck.set_crossfade(at(crossfade));
        let mut first = bench.tone(1, Some(at(48.0)));
        first.end_at = Some(at(51.0));
        bench.deck.play(first);
        let second = bench.tone(2, Some(at(10.0)));
        bench.deck.set_next(Some(second));
        bench.wait_for_next(10.0);
        bench
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

fn peak(audio: &[f32]) -> f32 {
    audio.iter().fold(0.0f32, |m, s| m.max(s.abs()))
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
    // past the few milliseconds the volume takes to change
    let half = bench.render(0.25).split_off(at(0.05) as usize * 2);
    bench.deck.play(bench.tone(2, None));
    bench.deck.stop();
    assert_eq!(
        ids(&bench.deck.drain_events()),
        ["start 1", "end 1 Replaced", "start 2", "end 2 Stopped"]
    );
    assert!(bench.deck.is_idle());

    let ratio = peak(&half) / peak(&full);
    assert!(
        (0.47..=0.53).contains(&ratio),
        "volume 0.5 scaled the peak by {ratio}"
    );

    // the replaced track fades out instead of stopping dead, then there is silence
    let mut fading = vec![0.0; BLOCK_FRAMES * 2];
    bench.deck.render(&mut fading);
    assert!(peak(&fading[..200]) > 0.1, "the cut-off track fades out");
    assert!(
        peak(&fading[fading.len() - 20..]) < 0.01,
        "and is gone by the end"
    );
    assert!(!bench.deck.is_audible());
    let mut silence = vec![1.0; 1920];
    bench.deck.render(&mut silence);
    assert!(
        silence.iter().all(|&s| s == 0.0),
        "an idle deck outputs silence"
    );
}

#[test]
fn volume_changes_ramp_instead_of_jumping() {
    let mut bench = Bench::new();
    bench.deck.play(bench.tone(1, None));
    bench.render(0.2);
    // the tone's amplitude is constant, so the peak of each period (5 ms at the start, 200 Hz)
    // shows the gain
    bench.deck.set_volume(0.0);
    let audio = bench.render(0.04);
    let peaks: Vec<f32> = audio.chunks(240 * 2).map(peak).collect();
    assert!(peaks[0] > 0.5, "{peaks:?}");
    assert!(
        peaks.windows(2).all(|w| w[0] - w[1] < 0.4),
        "the gain fell faster than a ramp: {peaks:?}"
    );
    assert!(peaks[5..].iter().all(|&p| p < 1e-3), "{peaks:?}");
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

/// A source that holds its media back until `until`, and never sends any.
struct Withheld {
    until: Instant,
}

impl Producer for Withheld {
    fn caps(&self) -> ProducerCaps {
        ProducerCaps::default()
    }

    fn open(&self, _start: Option<Ticks>) -> BoxFuture<'static, io::Result<ProducerStream>> {
        Box::pin(std::future::pending())
    }

    fn available_at(&self) -> Option<Instant> {
        Some(self.until)
    }
}

#[test]
fn a_wait_the_source_announced_is_not_a_stall() {
    let mut bench = Bench::stuck_after(Duration::from_millis(100));
    let started = Instant::now();
    let until = started + Duration::from_millis(400);
    bench.deck.play(bench.track(7, Withheld { until }, None));
    let mut block = vec![0.0; BLOCK_FRAMES * 2];
    let mut render_until = |deadline: Instant| {
        while Instant::now() < deadline {
            bench.deck.render(&mut block);
            std::thread::sleep(Duration::from_millis(10));
        }
        bench.deck.drain_events()
    };

    // three times the threshold, but the source said it would take this long
    let events = render_until(until - Duration::from_millis(100));
    assert_eq!(ids(&events), ["start 7"]);

    let events = render_until(until + Duration::from_millis(300));
    assert_eq!(ids(&events), ["stuck 7"]);
    let DeckEvent::Stuck(_, waiting) = events[0] else {
        unreachable!()
    };
    assert!(
        waiting >= until - started,
        "the whole wait is reported: {waiting:?}"
    );
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

/// 0.1 s of `audio` from `from` seconds.
fn window(audio: &[f32], from: f64) -> &[f32] {
    &audio[at(from) as usize * 2..at(from + 0.1) as usize * 2]
}

#[test]
fn a_crossfade_overlaps_the_end_of_a_track_with_the_next() {
    let mut bench = Bench::crossfading(1.0);
    let audio = bench.render(4.0);
    // the switch is reported where the fade starts, in the same order as without one
    assert_eq!(
        ids(&bench.deck.drain_events()),
        ["start 1", "end 1 Finished", "start 2"]
    );

    // alone: the first track at 49 s (690 Hz)
    let alone = tone_level(window(&audio, 1.2), 690.0);
    assert!(alone > 0.3, "level {alone}");
    assert!(tone_level(window(&audio, 1.2), 300.0) < 0.05 * alone);
    // from 2 s on, its second at 50 s (700 Hz) fades out as the next track's 10 s (300 Hz)
    // fades in, keeping the power of one track
    let mut fading = Vec::new();
    for step in 0..10 {
        let w = window(&audio, 2.0 + f64::from(step) / 10.0);
        let (out, into) = (tone_level(w, 700.0), tone_level(w, 300.0));
        let power = (out * out + into * into) / (alone * alone);
        assert!(
            (0.708..=1.413).contains(&power),
            "{step}: power {power:.2} of one track ({out:.2} out, {into:.2} in)"
        );
        fading.push(out);
    }
    assert!(fading.windows(2).all(|w| w[1] < w[0]), "{fading:?}");
    // then only the next track, at 11 s (310 Hz)
    let after = window(&audio, 3.2);
    assert!(tone_level(after, 310.0) > 0.8 * alone);
    assert!(tone_level(after, 700.0) < 0.05 * alone);
}

#[test]
fn with_a_rate_a_crossfade_lasts_as_long_and_takes_more_of_the_track() {
    let mut bench = Bench::crossfading(1.0);
    let graph = GraphSpec {
        rate: Some(1.25),
        ..GraphSpec::default()
    };
    let effects = NodeRegistry::with_builtins()
        .compile(&graph, 48_000.0)
        .unwrap();
    bench.deck.set_effects(Some(effects));

    let mut block = vec![0.0; BLOCK_FRAMES * 2];
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let switched_at = loop {
        let before = bench.deck.current().unwrap().player.position();
        bench.deck.render(&mut block);
        if bench.deck.current().unwrap().id == 2 {
            break before;
        }
        assert!(std::time::Instant::now() < deadline, "no crossfade");
        std::thread::sleep(Duration::from_millis(1));
    };
    // a second of output takes 1.25 s of the track, so the fade starts 1.25 s before its end
    let expected = at(51.0 - 1.25);
    assert!(
        (switched_at - expected).abs() < at(0.05),
        "faded from {:.3} s",
        switched_at as f64 / 48_000.0
    );
}

#[test]
fn a_crossfade_takes_at_most_half_of_what_plays_from_the_tracks_start() {
    // the first track plays from 48 s to 51 s
    let mut bench = Bench::crossfading(2.5);
    let mut block = vec![0.0; BLOCK_FRAMES * 2];
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let switched_at = loop {
        let before = bench.deck.current().unwrap().player.position();
        bench.deck.render(&mut block);
        if bench.deck.current().unwrap().id == 2 {
            break before;
        }
        assert!(std::time::Instant::now() < deadline, "no crossfade");
        std::thread::sleep(Duration::from_millis(1));
    };
    let expected = at(51.0 - 1.5);
    assert!(
        (switched_at - expected).abs() < at(0.05),
        "faded from {:.3} s",
        switched_at as f64 / 48_000.0
    );
}

#[test]
fn a_next_track_that_is_not_ready_follows_without_a_crossfade() {
    let mut bench = Bench::new();
    bench.deck.set_crossfade(at(1.0));
    let mut first = bench.tone(1, Some(at(48.0)));
    first.end_at = Some(at(51.0));
    bench.deck.play(first);
    let stalled = ProcessProducer::new(ProducerCaps::default(), None, |_| {
        (ProcessSpec::new("sleep").arg("30"), TimestampMode::Absolute)
    });
    bench.deck.set_next(Some(bench.track(2, stalled, None)));

    let audio = bench.render(2.9);
    assert_eq!(ids(&bench.deck.drain_events()), ["start 1"]);
    // no fade: at 50.7 s the first track is as loud as at 49.2 s
    let alone = tone_level(window(&audio, 1.2), 690.0);
    assert!(tone_level(window(&audio, 2.7), 700.0) > 0.8 * alone);
    let mut block = vec![0.0; BLOCK_FRAMES * 2];
    for _ in 0..10 {
        bench.deck.render(&mut block);
    }
    assert_eq!(
        ids(&bench.deck.drain_events()),
        ["end 1 Finished", "start 2"]
    );
}

#[test]
fn skipping_during_a_crossfade_cuts_both_tracks_at_once() {
    let mut bench = Bench::crossfading(1.0);
    bench.render(2.5);
    bench.deck.drain_events();
    bench.deck.skip();
    assert_eq!(ids(&bench.deck.drain_events()), ["end 2 Stopped"]);
    let mut block = vec![0.0; BLOCK_FRAMES * 2];
    bench.deck.render(&mut block);
    assert!(peak(&block[..200]) > 0.05, "both fade out quickly");
    let mut after = vec![1.0; 240 * 2];
    bench.deck.render(&mut after);
    assert!(after.iter().all(|&s| s == 0.0), "and are gone after 20 ms");
    assert!(!bench.deck.is_audible());
}
