//! Shared by the integration tests: a test tone whose frequency tells which second of the
//! media is playing, and helpers to measure it.

#![allow(dead_code, unreachable_pub)] // each test binary uses a different subset

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use entroximer_core::timeline::{TICKS_PER_SECOND, Ticks};

pub const FIXTURE_SECONDS: i64 = 60;
pub const BLOCK_FRAMES: usize = 960;

pub fn fixture() -> &'static Path {
    static FIXTURE: OnceLock<PathBuf> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("steps.webm");
        if !path.exists() {
            let tone =
                format!("aevalsrc=sin(2*PI*(200+10*floor(t))*t):s=48000:d={FIXTURE_SECONDS}");
            let status = Command::new("ffmpeg")
                .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"])
                .args([
                    "-f", "lavfi", "-i", &tone, "-ac", "2", "-c:a", "libopus", "-b:a", "128k",
                ])
                .arg(&path)
                .status()
                .expect("ffmpeg must be installed to run these tests");
            assert!(status.success(), "ffmpeg failed to create the fixture");
        }
        path
    })
}

pub fn expected_hz(second: f64) -> f64 {
    200.0 + 10.0 * second.floor()
}

pub fn at(seconds: f64) -> Ticks {
    (seconds * TICKS_PER_SECOND as f64).round() as Ticks
}

/// Frequency of the left channel, from its zero crossings.
pub fn frequency(interleaved: &[f32]) -> f64 {
    let left: Vec<f32> = interleaved.iter().step_by(2).copied().collect();
    let crossings = left
        .windows(2)
        .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
        .count();
    crossings as f64 / 2.0 / (left.len() as f64 / 48_000.0)
}

pub fn assert_playing_second(audio: &[f32], second: f64) {
    let hz = frequency(audio);
    let expected = expected_hz(second);
    assert!(
        (hz - expected).abs() < 4.0,
        "expected second {second} ({expected} Hz), heard {hz:.1} Hz"
    );
}
