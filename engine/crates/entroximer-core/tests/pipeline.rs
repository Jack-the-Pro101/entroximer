//! End-to-end tests of producers, the timeline, decoding and seeking on real Opus/WebM media.
//!
//! The fixture is a 60 s tone whose frequency is `200 + 10 * second` Hz, so measuring the
//! frequency of the rendered audio tells which second of the media is playing. Requires ffmpeg
//! with libopus (the engine needs ffmpeg at runtime anyway).

use std::sync::Arc;
use std::time::{Duration, Instant};

use entroximer_core::error::display_chain;
use entroximer_core::player::{Player, PlayerConfig, Status};
use entroximer_core::producer::file::FileProducer;
use entroximer_core::producer::process::{ProcessProducer, ProcessSpec};
use entroximer_core::producer::{Producer, ProducerCaps, TimestampMode};
use entroximer_core::timeline::{SpillConfig, TICKS_PER_SECOND};

mod common;
use common::{BLOCK_FRAMES, FIXTURE_SECONDS, assert_playing_second, at, fixture};

/// ffmpeg streaming the fixture at real-time speed, like a network download would.
fn throttled(time_seek: bool) -> impl Producer {
    let input = fixture().to_string_lossy().into_owned();
    ProcessProducer::new(
        ProducerCaps {
            time_seek,
            live: false,
        },
        Some("mkv".into()),
        move |start| {
            let mut spec =
                ProcessSpec::new("ffmpeg").args(["-nostdin", "-loglevel", "error", "-re"]);
            // `-ss 0` would drop the Opus pre-skip packet, which starts before zero
            if let Some(start) = start.filter(|&t| time_seek && t > 0) {
                spec = spec
                    .arg("-ss")
                    .arg(format!("{:.3}", start as f64 / TICKS_PER_SECOND as f64));
            }
            let spec = spec
                .args(["-copyts", "-i", &input])
                .args(["-c", "copy", "-f", "matroska", "pipe:1"]);
            (spec, TimestampMode::Absolute)
        },
    )
}

/// Declared after the player so the runtime outlives it.
struct Harness {
    player: Player,
    _spill: tempfile::TempDir,
    _rt: tokio::runtime::Runtime,
}

impl Harness {
    fn new(producer: impl Producer) -> Self {
        Self::with_config(producer, PlayerConfig::default())
    }

    fn with_config(producer: impl Producer, mut cfg: PlayerConfig) -> Self {
        // `RUST_LOG=entroximer_core=debug cargo test` shows producer and coverage decisions
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let spill = tempfile::tempdir().unwrap();
        cfg.retention.spill = Some(SpillConfig {
            dir: spill.path().to_owned(),
            ..SpillConfig::default()
        });
        let player = Player::new(rt.handle().clone(), Arc::new(producer), cfg, None);
        Self {
            player,
            _spill: spill,
            _rt: rt,
        }
    }

    /// Renders exactly `seconds` of audio, waiting for media like an offline renderer would.
    fn render(&mut self, seconds: f64) -> Vec<f32> {
        let wanted = (seconds * 48_000.0).round() as usize;
        let mut out = Vec::with_capacity(wanted * 2);
        let mut block = vec![0.0; BLOCK_FRAMES * 2];
        let deadline = Instant::now() + Duration::from_secs(30);
        while out.len() < wanted * 2 {
            let want = (wanted - out.len() / 2).min(BLOCK_FRAMES);
            let rendered = self.player.render(&mut block[..want * 2]).unwrap();
            out.extend_from_slice(&block[..rendered.frames * 2]);
            match rendered.status {
                Status::Playing => {}
                Status::Buffering => self.player.wait_for_media(Duration::from_millis(50)),
                Status::Ended => panic!("media ended after {} frames", out.len() / 2),
            }
            assert!(Instant::now() < deadline, "rendering stalled");
        }
        out
    }

    fn render_to_end(&mut self) -> usize {
        let mut block = vec![0.0; BLOCK_FRAMES * 2];
        let mut frames = 0;
        loop {
            let rendered = self.player.render(&mut block).unwrap();
            frames += rendered.frames;
            match rendered.status {
                Status::Playing => {}
                Status::Buffering => self.player.wait_for_media(Duration::from_millis(50)),
                Status::Ended => return frames,
            }
        }
    }

    fn wait_until_fully_buffered(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.player.media_end().is_none() {
            assert!(Instant::now() < deadline, "download did not finish");
            self.player.wait_for_media(Duration::from_millis(50));
        }
    }
}

#[test]
fn decodes_the_whole_file_from_a_forward_only_stream() {
    let mut h = Harness::new(FileProducer {
        path: fixture().to_owned(),
    });
    let first = h.render(10.5);
    assert_playing_second(&first[at(10.3) as usize * 2..], 10.3);

    let total = at(10.5) as usize + h.render_to_end();
    let expected = (FIXTURE_SECONDS * TICKS_PER_SECOND) as usize;
    assert!(
        total.abs_diff(expected) < 960,
        "rendered {total} frames, expected about {expected}"
    );
}

#[test]
fn seeks_within_buffered_audio_are_instant_and_exact() {
    let mut h = Harness::new(FileProducer {
        path: fixture().to_owned(),
    });
    h.wait_until_fully_buffered();

    for second in [30.3, 5.3, 57.3] {
        h.player.seek(at(second)).unwrap();
        assert_eq!(h.player.position(), at(second));
        let started = Instant::now();
        let audio = h.render(0.2);
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_playing_second(&audio, second);
        assert_eq!(h.player.position(), at(second + 0.2));
    }
}

#[test]
fn far_seek_reopens_a_time_seekable_source() {
    let mut h = Harness::new(throttled(true));
    h.render(0.5);

    h.player.seek(at(40.3)).unwrap();
    let started = Instant::now();
    let audio = h.render(0.2);
    // waiting for a real-time download would take ~40 s
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );
    assert_playing_second(&audio, 40.3);
    assert_eq!(
        h.player.buffered().len(),
        2,
        "expected the start and the re-opened range"
    );
}

#[test]
fn near_seek_waits_for_the_download() {
    let mut h = Harness::new(throttled(true));
    h.render(0.2);

    h.player.seek(at(1.3)).unwrap();
    let audio = h.render(0.2);
    assert_playing_second(&audio, 1.3);
    assert_eq!(
        h.player.buffered().len(),
        1,
        "no second producer was needed"
    );
}

#[test]
fn forward_only_source_waits_instead_of_restarting() {
    let mut h = Harness::new(throttled(false));
    h.player.seek(at(2.3)).unwrap();
    let audio = h.render(0.2);
    assert_playing_second(&audio, 2.3);
    let buffered = h.player.buffered();
    assert_eq!(buffered.len(), 1);
    assert!(
        buffered[0].0 <= at(0.01),
        "the only range starts at the beginning"
    );
}

#[test]
fn failing_source_is_retried_then_reported_with_its_own_error() {
    let attempts = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let producer = ProcessProducer::new(ProducerCaps::default(), None, {
        let attempts = Arc::clone(&attempts);
        move |_| {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let fail =
                ProcessSpec::new("sh").args(["-c", "echo 'HTTP Error 403: Forbidden' >&2; exit 3"]);
            (fail, TimestampMode::Absolute)
        }
    });
    let cfg = PlayerConfig {
        max_producer_retries: 2,
        retry_backoff: Duration::from_millis(20),
        ..PlayerConfig::default()
    };
    let mut h = Harness::with_config(producer, cfg);

    let mut block = vec![0.0; BLOCK_FRAMES * 2];
    let deadline = Instant::now() + Duration::from_secs(10);
    let error = loop {
        match h.player.render(&mut block) {
            Ok(rendered) => assert_eq!(rendered.status, Status::Buffering),
            Err(e) => break e,
        }
        assert!(Instant::now() < deadline, "the failure was never reported");
        h.player.wait_for_media(Duration::from_millis(10));
    };
    let message = display_chain(&error);
    assert!(
        message.contains("403"),
        "the producer's error is lost: {message}"
    );
    assert_eq!(
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "first try plus two retries"
    );
}
