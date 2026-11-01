//! Node types shipped with the engine. All of them are stereo in, stereo out.

use std::f32::consts::{FRAC_1_SQRT_2, TAU};
use std::ops::RangeInclusive;
use std::sync::Arc;

use fundsp::audiounit::AudioUnit;
use fundsp::prelude32::{
    An, bandpass_hz, bell_hz, chorus, db_amp, delay, feedback, flanger, highpass_hz, highshelf_hz,
    lerp, limiter_stereo, lowpass_hz, lowshelf_hz, pass, phaser, reverb_stereo,
};

use super::nodes::{AutoPan, ChannelMix, Drive, PitchShift, Tremolo, Vibrato};
use super::{NodeDescriptor, NodeFactory, NodeRegistry, ParamSpec, ParamValues};

pub(super) fn register(registry: &mut NodeRegistry) {
    for node in builtins() {
        registry
            .register(Arc::new(node))
            .expect("built-in node types have unique names");
    }
}

struct BuiltinNode {
    descriptor: NodeDescriptor,
    build: fn(&ParamValues) -> Box<dyn AudioUnit>,
}

impl NodeFactory for BuiltinNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.descriptor
    }

    fn build(&self, params: &ParamValues) -> Box<dyn AudioUnit> {
        (self.build)(params)
    }
}

fn node<const N: usize>(
    kind: &str,
    description: &str,
    params: [ParamSpec; N],
    build: fn(&ParamValues) -> Box<dyn AudioUnit>,
) -> BuiltinNode {
    BuiltinNode {
        descriptor: NodeDescriptor {
            kind: kind.into(),
            description: description.into(),
            params: params.into(),
        },
        build,
    }
}

fn param(name: &str, description: &str, default: f32, range: RangeInclusive<f32>) -> ParamSpec {
    ParamSpec {
        name: name.into(),
        description: description.into(),
        default,
        min: *range.start(),
        max: *range.end(),
    }
}

fn cutoff(default: f32) -> ParamSpec {
    param(
        "cutoff",
        "Cutoff frequency in Hz.",
        default,
        20.0..=20_000.0,
    )
}

fn resonance() -> ParamSpec {
    param("q", "Resonance.", FRAC_1_SQRT_2, 0.1..=20.0)
}

fn shelf_gain() -> ParamSpec {
    param(
        "gain_db",
        "Boost (positive) or cut (negative) in dB.",
        0.0,
        -24.0..=24.0,
    )
}

fn modulation_frequency(default: f32, max: f32) -> ParamSpec {
    param(
        "frequency",
        "Modulation frequency in Hz.",
        default,
        0.01..=max,
    )
}

fn mix(default: f32) -> ParamSpec {
    param(
        "mix",
        "Share of the processed signal: 0 is dry, 1 fully processed.",
        default,
        0.0..=1.0,
    )
}

fn builtins() -> Vec<BuiltinNode> {
    vec![
        node(
            "volume",
            "Scales the signal by a linear gain.",
            [param(
                "gain",
                "Linear gain; 1 leaves the signal unchanged.",
                1.0,
                0.0..=5.0,
            )],
            |p| Box::new((pass() | pass()) * p.get("gain")),
        ),
        node(
            "reverb",
            "Stereo feedback-delay-network reverb.",
            [
                param("room_size", "Room size in meters.", 10.0, 1.0..=100.0),
                param(
                    "time",
                    "Reverberation time to -60 dB, in seconds.",
                    2.0,
                    0.1..=30.0,
                ),
                param("damping", "High frequency damping.", 0.5, 0.0..=1.0),
                mix(0.3),
            ],
            |p| {
                let wet = p.get("mix");
                let reverb = reverb_stereo(p.get("room_size"), p.get("time"), p.get("damping"));
                Box::new(((pass() | pass()) * (1.0 - wet)) & (reverb * wet))
            },
        ),
        node(
            "chorus",
            "Five-voice chorus, decorrelated between channels.",
            [
                param(
                    "separation",
                    "Delay between voices, in seconds.",
                    0.015,
                    0.0..=0.1,
                ),
                param(
                    "variation",
                    "Delay modulation depth, in seconds.",
                    0.005,
                    0.0..=0.1,
                ),
                param(
                    "mod_frequency",
                    "Delay modulation frequency, in Hz.",
                    0.2,
                    0.0..=10.0,
                ),
            ],
            |p| {
                let (sep, var, freq) = (
                    p.get("separation"),
                    p.get("variation"),
                    p.get("mod_frequency"),
                );
                Box::new(chorus(0, sep, var, freq) | chorus(1, sep, var, freq))
            },
        ),
        node(
            "echo",
            "Repeats the signal after a delay, each repeat quieter than the last.",
            [
                param(
                    "delay",
                    "Time between repeats, in seconds.",
                    0.3,
                    0.01..=2.0,
                ),
                param(
                    "feedback",
                    "How much of each repeat comes back in the next.",
                    0.4,
                    0.0..=0.95,
                ),
                mix(0.5),
            ],
            |p| {
                let (time, amount, wet) = (p.get("delay"), p.get("feedback"), p.get("mix"));
                let channel =
                    move || pass() & ((delay(time) >> feedback(delay(time) * amount)) * wet);
                Box::new(channel() | channel())
            },
        ),
        node(
            "lowpass",
            "Removes content above the cutoff frequency.",
            [cutoff(1000.0), resonance()],
            |p| {
                let (cutoff, q) = (p.get("cutoff"), p.get("q"));
                Box::new(lowpass_hz(cutoff, q) | lowpass_hz(cutoff, q))
            },
        ),
        node(
            "highpass",
            "Removes content below the cutoff frequency.",
            [cutoff(100.0), resonance()],
            |p| {
                let (cutoff, q) = (p.get("cutoff"), p.get("q"));
                Box::new(highpass_hz(cutoff, q) | highpass_hz(cutoff, q))
            },
        ),
        node(
            "bandpass",
            "Keeps a band around the center frequency and removes the rest.",
            [
                param("center", "Center frequency in Hz.", 1000.0, 20.0..=20_000.0),
                param("q", "Bandwidth; higher is narrower.", 0.7, 0.1..=20.0),
            ],
            |p| {
                let (center, q) = (p.get("center"), p.get("q"));
                Box::new(bandpass_hz(center, q) | bandpass_hz(center, q))
            },
        ),
        node(
            "bell",
            "Boosts or cuts a band around a center frequency (one equalizer band).",
            [
                param("center", "Center frequency in Hz.", 1000.0, 20.0..=20_000.0),
                param("q", "Bandwidth; higher is narrower.", 1.0, 0.1..=20.0),
                shelf_gain(),
            ],
            |p| {
                let (center, q, gain) = (p.get("center"), p.get("q"), db_amp(p.get("gain_db")));
                Box::new(bell_hz(center, q, gain) | bell_hz(center, q, gain))
            },
        ),
        node(
            "lowshelf",
            "Boosts or cuts everything below the cutoff frequency, e.g. for more bass.",
            [cutoff(120.0), resonance(), shelf_gain()],
            |p| {
                let (cutoff, q, gain) = (p.get("cutoff"), p.get("q"), db_amp(p.get("gain_db")));
                Box::new(lowshelf_hz(cutoff, q, gain) | lowshelf_hz(cutoff, q, gain))
            },
        ),
        node(
            "highshelf",
            "Boosts or cuts everything above the cutoff frequency, e.g. for more treble.",
            [cutoff(6000.0), resonance(), shelf_gain()],
            |p| {
                let (cutoff, q, gain) = (p.get("cutoff"), p.get("q"), db_amp(p.get("gain_db")));
                Box::new(highshelf_hz(cutoff, q, gain) | highshelf_hz(cutoff, q, gain))
            },
        ),
        node(
            "tremolo",
            "Raises and lowers the volume periodically.",
            [
                modulation_frequency(4.0, 20.0),
                param(
                    "depth",
                    "How far the volume dips: 1 goes down to silence.",
                    0.5,
                    0.0..=1.0,
                ),
            ],
            |p| Box::new(An(Tremolo::new(p.get("frequency"), p.get("depth")))),
        ),
        node(
            "vibrato",
            "Wobbles the pitch periodically.",
            [
                modulation_frequency(5.0, 14.0),
                param(
                    "depth",
                    "Strength: 1 swings about a semitone and a half either way at 5 Hz.",
                    0.5,
                    0.0..=1.0,
                ),
            ],
            |p| Box::new(An(Vibrato::new(p.get("frequency"), p.get("depth")))),
        ),
        node(
            "auto_pan",
            "Moves the sound from side to side (\"8D audio\"), keeping its loudness even.",
            [
                modulation_frequency(0.15, 5.0),
                param(
                    "depth",
                    "How far towards each side: 1 reaches all the way.",
                    1.0,
                    0.0..=1.0,
                ),
            ],
            |p| Box::new(An(AutoPan::new(p.get("frequency"), p.get("depth")))),
        ),
        node(
            "channel_mix",
            "Mixes each output channel from both input channels: mono (all 0.5), swapped channels, \
             or their difference, which removes what they share such as centered vocals.",
            [
                param(
                    "left_to_left",
                    "Left input into the left output.",
                    1.0,
                    -2.0..=2.0,
                ),
                param(
                    "left_to_right",
                    "Left input into the right output.",
                    0.0,
                    -2.0..=2.0,
                ),
                param(
                    "right_to_left",
                    "Right input into the left output.",
                    0.0,
                    -2.0..=2.0,
                ),
                param(
                    "right_to_right",
                    "Right input into the right output.",
                    1.0,
                    -2.0..=2.0,
                ),
            ],
            |p| {
                Box::new(An(ChannelMix {
                    left_to_left: p.get("left_to_left"),
                    left_to_right: p.get("left_to_right"),
                    right_to_left: p.get("right_to_left"),
                    right_to_right: p.get("right_to_right"),
                }))
            },
        ),
        node(
            "drive",
            "Saturates the signal: warmth at low drive, distortion at high drive.",
            [
                param("drive", "Amount; 1 is barely audible.", 4.0, 1.0..=50.0),
                mix(1.0),
            ],
            |p| Box::new(An(Drive::new(p.get("drive"), p.get("mix")))),
        ),
        node(
            "phaser",
            "Sweeps notches through the spectrum for a whooshing sound.",
            [
                modulation_frequency(0.5, 10.0),
                param(
                    "feedback",
                    "Intensity; negative inverts it.",
                    0.5,
                    -0.95..=0.95,
                ),
                param("depth", "How far the notches sweep.", 1.0, 0.0..=1.0),
            ],
            |p| {
                let (frequency, amount, depth) =
                    (p.get("frequency"), p.get("feedback"), p.get("depth"));
                // the channels sweep a quarter cycle apart, for width
                let sweep = move |offset: f32| {
                    move |t: f32| 0.5 + 0.5 * depth * (TAU * (frequency * t + offset)).sin()
                };
                Box::new(phaser(amount, sweep(0.0)) | phaser(amount, sweep(0.25)))
            },
        ),
        node(
            "flanger",
            "Mixes in a copy of the signal through a short sweeping delay, for a jet-like sound.",
            [
                modulation_frequency(0.2, 10.0),
                param(
                    "feedback",
                    "Intensity; negative inverts it.",
                    0.5,
                    -0.95..=0.95,
                ),
                param(
                    "depth",
                    "How far the delay sweeps, up to 10 ms.",
                    0.5,
                    0.0..=1.0,
                ),
            ],
            |p| {
                const MIN_DELAY: f32 = 0.001;
                let (frequency, amount) = (p.get("frequency"), p.get("feedback"));
                let max_delay = MIN_DELAY + p.get("depth") * 0.009;
                let sweep = move |offset: f32| {
                    move |t: f32| {
                        let position = 0.5 + 0.5 * (TAU * (frequency * t + offset)).sin();
                        lerp(MIN_DELAY, max_delay, position)
                    }
                };
                Box::new(
                    flanger(amount, MIN_DELAY, max_delay, sweep(0.0))
                        | flanger(amount, MIN_DELAY, max_delay, sweep(0.25)),
                )
            },
        ),
        node(
            "pitch",
            "Shifts the pitch without changing the speed.",
            [param(
                "semitones",
                "Shift in semitones; 12 is an octave up.",
                0.0,
                -12.0..=12.0,
            )],
            |p| Box::new(An(PitchShift::new(p.get("semitones")))),
        ),
        node(
            "limiter",
            "Keeps peaks within full scale, e.g. after boosting. Delays the signal by its attack \
             time.",
            [
                param("attack", "Look-ahead in seconds.", 0.005, 0.001..=0.1),
                param("release", "Recovery time in seconds.", 0.25, 0.01..=2.0),
            ],
            |p| Box::new(limiter_stereo(p.get("attack"), p.get("release"))),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, Value};

    use super::*;
    use crate::dsp::{GraphSpec, NodeSpec};

    #[test]
    fn every_node_builds_with_defaults_and_extremes_and_stays_finite() {
        let registry = NodeRegistry::with_builtins();
        for descriptor in registry.descriptors() {
            let at = |pick: fn(&ParamSpec) -> f32| -> Map<String, Value> {
                descriptor
                    .params
                    .iter()
                    .map(|p| (p.name.clone(), Value::from(f64::from(pick(p)))))
                    .collect()
            };
            for params in [Map::new(), at(|p| p.min), at(|p| p.max)] {
                let spec = GraphSpec {
                    nodes: vec![NodeSpec {
                        id: "node".into(),
                        kind: descriptor.kind.clone(),
                        params,
                    }],
                    ..GraphSpec::default()
                };
                let mut effects = registry.compile(&spec, 48_000.0).unwrap();
                let mut out = vec![0.0; 960 * 2];
                let mut t = 0;
                for _ in 0..50 {
                    effects.render(&mut out, |source| {
                        for frame in source.as_chunks_mut::<2>().0 {
                            frame.fill((TAU * 440.0 * t as f32 / 48_000.0).sin() * 0.8);
                            t += 1;
                        }
                    });
                    assert!(
                        out.iter().all(|s| s.is_finite() && s.abs() < 50.0),
                        "{} blew up",
                        descriptor.kind
                    );
                }
            }
        }
    }
}
