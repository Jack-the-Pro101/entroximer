//! Node types shipped with the engine. All of them are stereo in, stereo out.

use std::f32::consts::{FRAC_1_SQRT_2, FRAC_PI_2, TAU};
use std::ops::RangeInclusive;
use std::sync::Arc;

use fundsp::audiounit::AudioUnit;
use fundsp::prelude32::{
    An, allnest_c, bandpass_hz, bell_hz, chorus, db_amp, delay, feedback, flanger, highpass_hz,
    highshelf_hz, lerp, limiter_stereo, lowpass_hz, lowshelf_hz, pass, phaser, reverb_stereo,
};

use super::nodes::{
    AutoPan, ChannelMix, Compressor, Drive, Orbit, PitchShift, Tremolo, Vibrato, Vinyl,
};
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

/// The gain that brings `reverb_stereo`'s output about as loud as its input.
///
/// Its feedback network loses a share `1 - a²` of its energy on every pass, so what it puts out
/// adds up to `a² / (1 - a²)` of an eighth of what goes in (the input feeds 16 lines per channel,
/// summed at 1/16). Measured on music, the output is 11 dB below the input times that, within a
/// dB across rooms and times. Short reverberations in large rooms barely sound at all; they are
/// raised by at most 12 dB.
fn reverb_level(room_size: f32, time: f32) -> f32 {
    // as fundsp computes the feedback gain `a`
    let a_squared = 10f32.powf(-0.018 * room_size / time);
    let gain = 10f32.powf(11.2 / 20.0) * ((1.0 - a_squared) / a_squared).sqrt();
    gain.min(4.0)
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
            "Stereo feedback-delay-network reverb. The reverberated signal alone (`mix` 1) is about \
             as loud as the input, whatever the room, and the mix keeps the loudness even.",
            [
                param(
                    "room_size",
                    "Room size in meters; 10 to 30 sound best.",
                    10.0,
                    1.0..=100.0,
                ),
                param(
                    "time",
                    "Reverberation time to -60 dB, in seconds.",
                    2.0,
                    0.1..=30.0,
                ),
                param("damping", "High frequency damping.", 0.5, 0.0..=1.0),
                mix(0.25),
                param(
                    "pre_delay",
                    "Seconds before the reverberation starts; keeps it apart from the sound.",
                    0.01,
                    0.0..=0.2,
                ),
                param(
                    "low_cut",
                    "Frequency in Hz below which nothing is reverberated, so the bass stays clean.",
                    100.0,
                    20.0..=1000.0,
                ),
            ],
            |p| {
                let (room_size, time) = (p.get("room_size"), p.get("time"));
                // equal power, as the reverberation is uncorrelated with the dry signal
                let angle = p.get("mix") * FRAC_PI_2;
                let (dry, wet) = (angle.cos(), angle.sin() * reverb_level(room_size, time));
                let (pre_delay, low_cut) = (p.get("pre_delay"), p.get("low_cut"));
                let highs =
                    highpass_hz(low_cut, FRAC_1_SQRT_2) | highpass_hz(low_cut, FRAC_1_SQRT_2);
                // smear the input into a dense cloud first, so the reverberation does not start
                // with distinct echoes (fundsp's `reverb2_stereo` diffusers, in samples at 44.1 kHz)
                let diffuser = |first: f32, second: f32| {
                    delay(pre_delay)
                        >> allnest_c(0.618, delay(first / 44_100.0))
                        >> allnest_c(0.618, delay(second / 44_100.0))
                };
                let reverb = (diffuser(224.0, 340.0) | diffuser(247.0, 366.0))
                    >> reverb_stereo(room_size, time, p.get("damping"));
                // only the highs are crossfaded: the bass below the cut passes as it is
                let crossfade = ((pass() | pass()) * (dry - 1.0)) & (reverb * wet);
                Box::new((pass() | pass()) & (highs >> crossfade))
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
                param("frequency", "Modulation frequency in Hz.", 5.0, 0.1..=14.0),
                param(
                    "depth",
                    "How far the pitch swings either way, in semitones.",
                    0.5,
                    0.0..=1.0,
                ),
            ],
            |p| Box::new(An(Vibrato::new(p.get("frequency"), p.get("depth")))),
        ),
        node(
            "auto_pan",
            "Moves the sound from side to side, keeping its loudness even; `orbit` circles it \
             around the head instead.",
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
            "orbit",
            "Circles the sound around the listener's head (\"8D audio\"); best on headphones. The \
             middle of the stereo image orbits as one source: the far ear hears it later, quieter \
             and duller, and from behind it sounds duller still.",
            [
                param("frequency", "Rounds per second.", 0.15, 0.01..=2.0),
                param(
                    "depth",
                    "How far towards each side it goes: 1 reaches all the way.",
                    0.8,
                    0.0..=1.0,
                ),
                param(
                    "width",
                    "How much of the stereo image stays where it is: 0 orbits a single point, 1 \
                     keeps all of it.",
                    0.3,
                    0.0..=1.0,
                ),
            ],
            |p| {
                Box::new(An(Orbit::new(
                    p.get("frequency"),
                    p.get("depth"),
                    p.get("width"),
                )))
            },
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
            "compressor",
            "Evens out the level: whatever rises above the threshold is turned down by the ratio, \
             so the quiet parts come forward and the whole can be turned up.",
            [
                param(
                    "threshold",
                    "Level in dB (0 is full scale) above which it turns down.",
                    -18.0,
                    -60.0..=0.0,
                ),
                param(
                    "ratio",
                    "How much it turns down: at 4, every 4 dB above the threshold become 1.",
                    4.0,
                    1.0..=20.0,
                ),
                param(
                    "attack",
                    "Seconds it takes to turn down; longer lets the start of a beat through.",
                    0.01,
                    0.0001..=0.5,
                ),
                param(
                    "release",
                    "Seconds it takes to come back up.",
                    0.15,
                    0.01..=2.0,
                ),
                param("makeup", "Gain in dB applied afterwards.", 0.0, 0.0..=24.0),
            ],
            |p| {
                Box::new(An(Compressor::new(
                    p.get("threshold"),
                    p.get("ratio"),
                    p.get("attack"),
                    p.get("release"),
                    p.get("makeup"),
                )))
            },
        ),
        node(
            "vinyl",
            "Adds the surface noise of a worn record: a soft hiss and random crackles.",
            [
                param(
                    "amount",
                    "How loud the noise is: 1 is a badly worn record.",
                    0.5,
                    0.0..=1.0,
                ),
                param("density", "Crackles per second.", 8.0, 0.0..=100.0),
            ],
            |p| Box::new(An(Vinyl::new(p.get("amount"), p.get("density")))),
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
    use serde_json::{Map, Value, json};

    use super::*;
    use crate::dsp::{Effects, GraphSpec, NodeSpec};

    fn one_node(kind: &str, params: Value) -> Effects {
        let spec = GraphSpec {
            nodes: vec![NodeSpec {
                id: "node".into(),
                kind: kind.into(),
                params: serde_json::from_value(params).unwrap(),
            }],
            ..GraphSpec::default()
        };
        NodeRegistry::with_builtins()
            .compile(&spec, 48_000.0)
            .unwrap()
    }

    fn level_db(audio: &[f32]) -> f32 {
        10.0 * (audio.iter().map(|s| s * s).sum::<f32>() / audio.len() as f32).log10()
    }

    #[test]
    fn reverberation_alone_is_about_as_loud_as_its_input() {
        for (room_size, time) in [(10.0, 0.5), (10.0, 2.0), (20.0, 3.0), (30.0, 6.0)] {
            let mut reverb = one_node(
                "reverb",
                json!({ "room_size": room_size, "time": time, "damping": 0, "mix": 1, "low_cut": 20 }),
            );
            let mut state = 0x1234_5678_u32;
            let mut out = vec![0.0; 48_000 * 2 * 4];
            reverb.render(&mut out, |input| {
                for sample in input {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    *sample = state as f32 / u32::MAX as f32 - 0.5;
                }
            });
            // uniform noise in -0.5..0.5; the reverberation has built up after 2 s
            let change = level_db(&out[48_000 * 2 * 2..]) - 10.0 * (1.0f32 / 12.0).log10();
            assert!(
                change.abs() < 3.0,
                "a {room_size} m room ringing {time} s changes the level by {change:+.1} dB"
            );
        }
    }

    #[test]
    fn reverb_leaves_the_bass_below_its_cut_alone() {
        let mut reverb = one_node("reverb", json!({ "mix": 1, "low_cut": 400 }));
        let mut out = vec![0.0; 48_000 * 2];
        let mut t = 0;
        reverb.render(&mut out, |input| {
            for frame in input.as_chunks_mut::<2>().0 {
                frame.fill(0.5 * (TAU * 50.0 * t as f32 / 48_000.0).sin());
                t += 1;
            }
        });
        // a sine of amplitude 0.5 has a power of 1/8
        let change = level_db(&out[48_000..]) - 10.0 * (1.0f32 / 8.0).log10();
        assert!(change.abs() < 1.0, "50 Hz changed by {change:+.1} dB");
    }

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
