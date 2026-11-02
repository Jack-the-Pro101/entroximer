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

/// Amplitude of the left channel's component at `hz`, by the Goertzel algorithm: unlike zero
/// crossings, it tells two tones playing at once apart.
pub fn tone_level(interleaved: &[f32], hz: f64) -> f64 {
    let left: Vec<f64> = interleaved
        .iter()
        .step_by(2)
        .map(|&s| f64::from(s))
        .collect();
    let coefficient = 2.0 * (2.0 * std::f64::consts::PI * hz / 48_000.0).cos();
    let (mut previous, mut before) = (0.0, 0.0);
    for sample in &left {
        let current = sample + coefficient * previous - before;
        before = previous;
        previous = current;
    }
    let power = previous * previous + before * before - coefficient * previous * before;
    2.0 * power.sqrt() / left.len() as f64
}

/// A tone of `hz` at `amplitude` (linear) lasting `seconds`, made once per run.
pub fn tone(hz: u32, amplitude: f64, seconds: u32) -> PathBuf {
    static MAKING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let path = dir.join(format!("stereo-tone-{hz}-{amplitude}-{seconds}.webm"));
    let _one_at_a_time = MAKING.lock().unwrap_or_else(|e| e.into_inner());
    if !path.exists() {
        // under another name first, so that no test reads it half written
        let partial = dir.join(format!(
            "stereo-tone-{hz}-{amplitude}-{seconds}.{}.webm",
            std::process::id()
        ));
        // both channels at the amplitude: upmixing mono would put them 3 dB lower
        let channel = format!("{amplitude}*sin(2*PI*{hz}*t)");
        let tone = format!("aevalsrc={channel}|{channel}:s=48000:d={seconds}");
        let status = Command::new("ffmpeg")
            .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"])
            .args([
                "-f", "lavfi", "-i", &tone, "-c:a", "libopus", "-b:a", "128k",
            ])
            .arg(&partial)
            .status()
            .expect("ffmpeg must be installed to run these tests");
        assert!(status.success(), "ffmpeg failed to create a tone");
        std::fs::rename(&partial, &path).unwrap();
    }
    path
}
