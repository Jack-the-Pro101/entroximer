//! The mixer: layers with their own effects and volume that pause on their own, clips that queue,
//! overlap or replace each other, ducking, and a sum kept within full scale.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use entroximer_core::deck::{Clip, ClipMode, DeckEvent, DeckTrack, EndReason, Placed};
use entroximer_core::dsp::{
    GraphSpec, NodeDescriptor, NodeFactory, NodeRegistry, NodeSpec, ParamValues,
};
use entroximer_core::mixer::{Duck, Mixer};
use entroximer_core::player::{Player, PlayerConfig};
use entroximer_core::producer::ffmpeg::FfmpegProducer;
use entroximer_core::producer::process::{ProcessProducer, ProcessSpec};
use entroximer_core::producer::{Producer, ProducerCaps, TimestampMode};
use entroximer_core::timeline::Ticks;
use fundsp::audiounit::AudioUnit;
use fundsp::prelude::pass;

mod common;
use common::{BLOCK_FRAMES, at, fixture, tone, tone_level};

struct Bench {
    mixer: Mixer,
    rt: tokio::runtime::Runtime,
}

impl Bench {
    fn new() -> Self {
        Self {
            mixer: Mixer::new(Duration::from_secs(10)),
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

    fn file(&self, id: u64, path: &Path, start: Option<Ticks>) -> DeckTrack {
        self.track(
            id,
            FfmpegProducer::new("ffmpeg", path.to_string_lossy()),
            start,
        )
    }

    /// `hz` at amplitude 0.5 for `seconds`, as a clip at full volume.
    fn clip(&self, id: u64, hz: u32, seconds: f64) -> Clip {
        let mut track = self.file(id, &tone(hz, 0.5, 10), None);
        track.end_at = Some(at(seconds));
        Clip {
            track,
            volume: 1.0,
            fade_in: 0,
            fade_out: 0,
            looping: false,
        }
    }

    fn main_position(&self) -> Ticks {
        self.mixer.main().deck.current().unwrap().player.position()
    }

    /// Waits until every track has downloaded, so that rendering never waits for media.
    fn downloaded(&self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !self.mixer.players().all(|p| p.media_end().is_some()) {
            assert!(
                std::time::Instant::now() < deadline,
                "a track never downloaded"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn render(&mut self, seconds: f64) -> Vec<f32> {
        let mut out = vec![0.0; at(seconds) as usize * 2];
        for block in out.chunks_mut(BLOCK_FRAMES * 2) {
            self.mixer.render(block);
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

/// 0.1 s of `audio` from `from` seconds.
fn window(audio: &[f32], from: f64) -> &[f32] {
    &audio[at(from) as usize * 2..at(from + 0.1) as usize * 2]
}

fn near(level: f64, expected: f64) -> bool {
    (level - expected).abs() < 0.05
}

#[test]
fn a_layer_has_its_own_effects_and_volume() {
    let mut bench = Bench::new();
    // the music, twice as fast and at a tenth of its volume
    let rate = GraphSpec {
        rate: Some(2.0),
        ..GraphSpec::default()
    };
    let main = &mut bench.mixer.main_mut().deck;
    main.set_effects(Some(
        NodeRegistry::with_builtins()
            .compile(&rate, 48_000.0)
            .unwrap(),
    ));
    main.set_volume(0.1);
    let music = bench.file(1, fixture(), Some(at(10.0)));
    bench.mixer.main_mut().deck.play(music);
    let clip = bench.clip(2, 1000, 5.0);
    let sfx = &mut bench.mixer.layer_or_add("sfx").deck;
    assert_eq!(sfx.place(clip, ClipMode::Mix(8)).unwrap(), Placed::Playing);
    bench.downloaded();

    let audio = bench.render(1.0);
    // through the music's rate the clip would be at 2 kHz, through its volume at 0.05
    let heard = window(&audio, 0.5);
    let clip = tone_level(heard, 1000.0);
    assert!(near(clip, 0.5), "{clip}");
    assert!(tone_level(heard, 2000.0) < 0.01);
    // the music plays at its own rate and volume: 11 s of the steps (310 Hz), twice as high
    let music = tone_level(heard, 620.0);
    assert!(near(music, 0.1), "{music}");
}

#[test]
fn pausing_the_music_leaves_other_layers_playing() {
    let mut bench = Bench::new();
    let music = bench.file(1, fixture(), None);
    bench.mixer.main_mut().deck.play(music);
    let clip = bench.clip(2, 1000, 5.0);
    let sfx = &mut bench.mixer.layer_or_add("sfx").deck;
    sfx.place(clip, ClipMode::Queue).unwrap();
    bench.downloaded();
    bench.render(0.2);

    bench.mixer.main_mut().paused = true;
    let held = bench.main_position();
    assert!(bench.mixer.is_audible(), "the clip still plays");
    let audio = bench.render(0.5);
    assert_eq!(bench.main_position(), held);
    assert!(near(tone_level(window(&audio, 0.3), 1000.0), 0.5));
    assert!(
        tone_level(window(&audio, 0.3), 200.0) < 0.01,
        "the music is not heard"
    );
}

#[test]
fn queued_clips_play_one_after_another_without_a_gap() {
    let mut bench = Bench::new();
    let (first, second, third) = (
        bench.clip(1, 1000, 0.5),
        bench.clip(2, 1000, 0.5),
        bench.clip(3, 1000, 0.5),
    );
    let tts = &mut bench.mixer.layer_or_add("tts").deck;
    assert_eq!(tts.place(first, ClipMode::Queue).unwrap(), Placed::Playing);
    assert_eq!(tts.place(second, ClipMode::Queue).unwrap(), Placed::Next);
    assert!(
        tts.place(third, ClipMode::Queue).is_err(),
        "only one clip waits"
    );
    bench.downloaded();

    let audio = bench.render(1.2);
    assert_eq!(
        ids(&bench.mixer.drain_events()),
        ["start 1", "end 1 Finished", "start 2", "end 2 Finished"]
    );
    // a sine crosses zero but never stays there; a gap would be a run of silent frames
    let switch = &audio[at(0.45) as usize * 2..at(0.55) as usize * 2];
    let longest_quiet = switch
        .chunks(2)
        .map(|f| f[0].abs() < 1e-4)
        .fold((0, 0), |(run, max), quiet| {
            let run = if quiet { run + 1 } else { 0 };
            (run, max.max(run))
        })
        .1;
    assert!(
        longest_quiet < 10,
        "{longest_quiet} silent frames between the clips"
    );
    assert!(
        tone_level(window(&audio, 1.05), 1000.0) < 0.01,
        "and then silence"
    );
}

#[test]
fn mixed_clips_overlap_and_the_oldest_makes_room() {
    let mut bench = Bench::new();
    // quiet enough that two at once stay below full scale, where the limiter would lower them
    let quiet = |clip: Clip| Clip {
        volume: 0.6,
        ..clip
    };
    let (a, b, c) = (
        quiet(bench.clip(1, 500, 5.0)),
        quiet(bench.clip(2, 1000, 5.0)),
        quiet(bench.clip(3, 1500, 5.0)),
    );
    let sfx = &mut bench.mixer.layer_or_add("sfx").deck;
    sfx.place(a, ClipMode::Mix(2)).unwrap();
    sfx.place(b, ClipMode::Mix(2)).unwrap();
    bench.downloaded();
    let both = bench.render(0.3);
    assert!(near(tone_level(window(&both, 0.1), 500.0), 0.3));
    assert!(near(tone_level(window(&both, 0.1), 1000.0), 0.3));

    let sfx = &mut bench.mixer.layer_mut("sfx").unwrap().deck;
    sfx.place(c, ClipMode::Mix(2)).unwrap();
    bench.downloaded();
    let after = bench.render(0.3);
    assert_eq!(
        ids(&bench.mixer.drain_events()),
        ["start 1", "start 2", "end 1 Replaced", "start 3"]
    );
    let heard = window(&after, 0.1);
    let levels = [500.0, 1000.0, 1500.0].map(|hz| tone_level(heard, hz));
    assert!(levels[0] < 0.01, "the oldest is gone: {levels:?}");
    assert!(near(levels[1], 0.3) && near(levels[2], 0.3), "{levels:?}");
}

#[test]
fn a_replacing_clip_ends_the_others_and_the_one_waiting() {
    let mut bench = Bench::new();
    let (a, b, c) = (
        bench.clip(1, 500, 5.0),
        bench.clip(2, 1000, 5.0),
        bench.clip(3, 1500, 5.0),
    );
    let voice = &mut bench.mixer.layer_or_add("voice").deck;
    voice.place(a, ClipMode::Queue).unwrap();
    voice.place(b, ClipMode::Queue).unwrap();
    voice.place(c, ClipMode::Replace).unwrap();
    assert!(voice.next().is_none());
    bench.downloaded();
    let audio = bench.render(0.3);
    assert_eq!(
        ids(&bench.mixer.drain_events()),
        ["start 1", "end 1 Replaced", "start 3"]
    );
    assert!(near(tone_level(window(&audio, 0.1), 1500.0), 0.5));
    assert!(tone_level(window(&audio, 0.1), 500.0) < 0.01);
}

fn voice_ducking_main(gain: f32) -> Duck {
    Duck {
        layers: vec!["main".to_owned()],
        gain,
        attack: at(0.05) as usize,
        release: at(0.5) as usize,
        hold: at(0.25) as usize,
    }
}

#[test]
fn ducking_lowers_the_music_while_a_clip_is_heard() {
    let mut bench = Bench::new();
    let music = bench.file(1, &tone(440, 0.5, 10), None);
    bench.mixer.main_mut().deck.play(music);
    // a clip still waiting for its media ducks nothing
    let stalled = ProcessProducer::new(ProducerCaps::default(), None, |_| {
        (ProcessSpec::new("sleep").arg("30"), TimestampMode::Absolute)
    });
    let waiting = Clip {
        track: bench.track(2, stalled, None),
        volume: 1.0,
        fade_in: 0,
        fade_out: 0,
        looping: false,
    };
    let voice = bench.mixer.layer_or_add("voice");
    voice.duck = Some(voice_ducking_main(0.25));
    voice.deck.place(waiting, ClipMode::Replace).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while bench
        .mixer
        .main()
        .deck
        .current()
        .unwrap()
        .player
        .media_end()
        .is_none()
    {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    let before = bench.render(0.3);
    assert!(near(tone_level(window(&before, 0.1), 440.0), 0.5));

    let clip = bench.clip(3, 1000, 1.0);
    let voice = &mut bench.mixer.layer_mut("voice").unwrap().deck;
    voice.place(clip, ClipMode::Replace).unwrap();
    bench.downloaded();
    let audio = bench.render(2.5);
    let music = |from: f64| tone_level(window(&audio, from), 440.0);
    assert!(
        near(music(0.1), 0.125),
        "ducked within the attack: {}",
        music(0.1)
    );
    assert!(near(music(0.8), 0.125));
    assert!(
        near(music(1.1), 0.125),
        "held after the clip: {}",
        music(1.1)
    );
    assert!(
        music(1.45) > 0.15 && music(1.45) < 0.45,
        "on its way back: {}",
        music(1.45)
    );
    assert!(
        near(music(1.9), 0.5),
        "back after the release: {}",
        music(1.9)
    );
    assert!(
        near(tone_level(window(&audio, 0.5), 1000.0), 0.5),
        "the clip itself is not ducked"
    );
}

#[test]
fn of_two_layers_ducking_the_music_the_lower_gain_wins() {
    let mut bench = Bench::new();
    let music = bench.file(1, &tone(440, 0.5, 10), None);
    bench.mixer.main_mut().deck.play(music);
    for (name, gain, id) in [("a", 0.5, 2), ("b", 0.2, 3)] {
        let clip = bench.clip(id, 1000, 5.0);
        let layer = bench.mixer.layer_or_add(name);
        layer.duck = Some(Duck {
            attack: 0,
            ..voice_ducking_main(gain)
        });
        layer.deck.place(clip, ClipMode::Queue).unwrap();
    }
    bench.downloaded();
    let audio = bench.render(0.5);
    let music = tone_level(window(&audio, 0.3), 440.0);
    assert!(near(music, 0.1), "{music}");
}

#[test]
fn the_sum_is_kept_within_full_scale() {
    let mut bench = Bench::new();
    let music = bench.file(1, &tone(440, 0.9, 10), None);
    bench.mixer.main_mut().deck.play(music);
    let clip = Clip {
        track: bench.file(2, &tone(440, 0.9, 10), None),
        volume: 1.0,
        fade_in: 0,
        fade_out: 0,
        looping: false,
    };
    let sfx = &mut bench.mixer.layer_or_add("sfx").deck;
    sfx.place(clip, ClipMode::Queue).unwrap();
    bench.downloaded();
    let audio = bench.render(1.0);
    let peak = audio.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(peak <= 1.0, "peak {peak}");
    assert!(peak > 0.9, "but not much quieter: {peak}");
}

#[test]
fn a_loop_plays_again_until_it_is_ended() {
    let mut bench = Bench::new();
    let mut clip = bench.clip(1, 1000, 0.3);
    clip.looping = true;
    let ambience = &mut bench.mixer.layer_or_add("ambience").deck;
    ambience.place(clip, ClipMode::Mix(8)).unwrap();
    bench.downloaded();
    let audio = bench.render(1.0);
    assert!(
        near(tone_level(window(&audio, 0.8), 1000.0), 0.5),
        "still playing at 0.8 s"
    );
    assert_eq!(ids(&bench.mixer.drain_events()), ["start 1"]);

    let ambience = &mut bench.mixer.layer_mut("ambience").unwrap().deck;
    assert!(ambience.end(1, EndReason::Stopped, None));
    let after = bench.render(0.2);
    assert_eq!(ids(&bench.mixer.drain_events()), ["end 1 Stopped"]);
    assert!(
        tone_level(window(&after, 0.05), 1000.0) < 0.01,
        "faded out at once"
    );
    assert!(!bench.mixer.is_audible());
}

#[test]
fn a_removed_layer_ends_its_clips_and_fades_out() {
    let mut bench = Bench::new();
    let clip = bench.clip(1, 1000, 5.0);
    let sfx = &mut bench.mixer.layer_or_add("sfx").deck;
    sfx.place(clip, ClipMode::Mix(8)).unwrap();
    bench.downloaded();
    bench.render(0.2);
    assert!(bench.mixer.remove_layer("sfx"));
    assert!(!bench.mixer.remove_layer("main"), "the music stays");
    assert!(bench.mixer.layer("sfx").is_none());
    let mut block = vec![0.0; BLOCK_FRAMES * 2];
    bench.mixer.render(&mut block);
    assert!(block.iter().any(|&s| s.abs() > 0.05), "it fades out");
    bench.mixer.render(&mut block);
    assert!(!bench.mixer.is_audible());
    assert_eq!(
        ids(&bench.mixer.drain_events()),
        ["start 1", "end 1 Stopped"]
    );
}

/// A node whose output is not a number.
struct Broken(NodeDescriptor);

impl NodeFactory for Broken {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.0
    }

    fn build(&self, _: &ParamValues) -> Box<dyn AudioUnit> {
        Box::new((pass() | pass()) * f32::NAN)
    }
}

#[test]
fn a_layer_that_breaks_leaves_the_others_alone() {
    let mut bench = Bench::new();
    let mut registry = NodeRegistry::with_builtins();
    registry
        .register(Arc::new(Broken(NodeDescriptor {
            kind: "broken".to_owned(),
            description: String::new(),
            params: Vec::new(),
        })))
        .unwrap();
    let graph = GraphSpec {
        nodes: vec![NodeSpec {
            id: "x".to_owned(),
            kind: "broken".to_owned(),
            params: Default::default(),
        }],
        ..GraphSpec::default()
    };
    let music = bench.file(1, &tone(440, 0.5, 10), None);
    bench.mixer.main_mut().deck.play(music);
    let clip = bench.clip(2, 1000, 5.0);
    let broken = &mut bench.mixer.layer_or_add("broken").deck;
    broken.set_effects(Some(registry.compile(&graph, 48_000.0).unwrap()));
    broken.place(clip, ClipMode::Queue).unwrap();
    bench.downloaded();

    let audio = bench.render(0.5);
    assert!(audio.iter().all(|s| s.is_finite()));
    let music = tone_level(window(&audio, 0.3), 440.0);
    assert!(near(music, 0.5), "the music plays on: {music}");
}

#[test]
fn a_paused_layer_holds_its_position_and_is_not_heard() {
    let mut bench = Bench::new();
    let track = bench.file(1, fixture(), None);
    bench.mixer.main_mut().deck.play(track);
    bench.downloaded();
    bench.render(0.3);
    assert!(bench.mixer.is_audible());

    bench.mixer.main_mut().paused = true;
    assert!(!bench.mixer.is_audible());
    let held = bench.main_position();
    let mut block = vec![1.0; BLOCK_FRAMES * 2];
    for _ in 0..10 {
        bench.mixer.render(&mut block);
        assert!(block.iter().all(|&s| s == 0.0), "a paused layer is silent");
    }
    assert_eq!(bench.main_position(), held);

    bench.mixer.main_mut().paused = false;
    bench.mixer.render(&mut block);
    assert_eq!(bench.main_position(), held + BLOCK_FRAMES as Ticks);
}
