//! Node types shipped with the engine. All of them are stereo in, stereo out.

use std::ops::RangeInclusive;
use std::sync::Arc;

use fundsp::audiounit::AudioUnit;
use fundsp::prelude32::{bell_hz, chorus, db_amp, highpass_hz, lowpass_hz, pass, reverb_stereo};

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
            ],
            |p| {
                Box::new(reverb_stereo(
                    p.get("room_size"),
                    p.get("time"),
                    p.get("damping"),
                ))
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
            "lowpass",
            "Removes content above the cutoff frequency.",
            [
                param("cutoff", "Cutoff frequency in Hz.", 1000.0, 20.0..=20_000.0),
                param(
                    "q",
                    "Resonance.",
                    std::f32::consts::FRAC_1_SQRT_2,
                    0.1..=20.0,
                ),
            ],
            |p| {
                let (cutoff, q) = (p.get("cutoff"), p.get("q"));
                Box::new(lowpass_hz(cutoff, q) | lowpass_hz(cutoff, q))
            },
        ),
        node(
            "highpass",
            "Removes content below the cutoff frequency.",
            [
                param("cutoff", "Cutoff frequency in Hz.", 100.0, 20.0..=20_000.0),
                param(
                    "q",
                    "Resonance.",
                    std::f32::consts::FRAC_1_SQRT_2,
                    0.1..=20.0,
                ),
            ],
            |p| {
                let (cutoff, q) = (p.get("cutoff"), p.get("q"));
                Box::new(highpass_hz(cutoff, q) | highpass_hz(cutoff, q))
            },
        ),
        node(
            "bell",
            "Boosts or cuts a band around a center frequency (one equalizer band).",
            [
                param("center", "Center frequency in Hz.", 1000.0, 20.0..=20_000.0),
                param("q", "Bandwidth; higher is narrower.", 1.0, 0.1..=20.0),
                param(
                    "gain_db",
                    "Boost (positive) or cut (negative) in dB.",
                    0.0,
                    -24.0..=24.0,
                ),
            ],
            |p| {
                let (center, q, gain) = (p.get("center"), p.get("q"), db_amp(p.get("gain_db")));
                Box::new(bell_hz(center, q, gain) | bell_hz(center, q, gain))
            },
        ),
    ]
}
