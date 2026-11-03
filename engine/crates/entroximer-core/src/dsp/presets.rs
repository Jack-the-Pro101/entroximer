//! Presets: ready-made graphs, such as nightcore or karaoke, built from the built-in node types.
//!
//! A preset expands into an ordinary [`GraphSpec`] whose nodes have readable ids (`bass`,
//! `reverb`, ...). A client can play it as it is, or take the graph and build on it: change a
//! node's parameters, or wire in nodes of its own. Presets with parameters (how much bass, how
//! fast) are validated like node parameters.

use std::collections::BTreeMap;
use std::ops::RangeInclusive;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{
    Edge, Endpoint, GRAPH_INPUT, GRAPH_OUTPUT, GraphSpec, NodeSpec, ParamError, ParamSpec,
    ParamSpecs, ParamValues,
};

/// A preset as clients see it.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PresetDescriptor {
    pub name: String,
    pub description: String,
    pub params: Vec<ParamSpec>,
    /// The graph the preset builds with its default parameters.
    pub graph: GraphSpec,
}

/// Which preset to build, and how.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresetSelection {
    pub name: String,
    /// Parameters by name; absent ones take their defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(type = "Record<string, number>", optional))]
    pub params: Option<Map<String, Value>>,
}

/// A preset the registry can build.
pub trait PresetFactory: Send + Sync {
    fn descriptor(&self) -> &PresetDescriptor;

    /// Builds the graph; `params` has been validated against `descriptor().params`.
    fn build(&self, params: &ParamValues) -> GraphSpec;
}

#[derive(Debug, thiserror::Error)]
pub enum PresetError {
    #[error("unknown preset `{0}`")]
    Unknown(String),
    #[error("invalid parameters for preset `{preset}`")]
    Param {
        preset: String,
        #[source]
        source: ParamError,
    },
    #[error("preset `{0}` is already registered")]
    Duplicate(String),
}

#[derive(Default)]
pub struct PresetRegistry {
    presets: BTreeMap<String, Arc<dyn PresetFactory>>,
}

impl PresetRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        for preset in builtins() {
            registry
                .register(Arc::new(preset))
                .expect("built-in presets have unique names");
        }
        registry
    }

    pub fn register(&mut self, preset: Arc<dyn PresetFactory>) -> Result<(), PresetError> {
        let name = preset.descriptor().name.clone();
        if self.presets.contains_key(&name) {
            return Err(PresetError::Duplicate(name));
        }
        self.presets.insert(name, preset);
        Ok(())
    }

    /// Builds the graph `selection` asks for.
    pub fn build(&self, selection: &PresetSelection) -> Result<GraphSpec, PresetError> {
        let preset = self
            .presets
            .get(&selection.name)
            .ok_or_else(|| PresetError::Unknown(selection.name.clone()))?;
        let params = preset
            .descriptor()
            .params
            .resolve(selection.params.as_ref().unwrap_or(&Map::new()))
            .map_err(|source| PresetError::Param {
                preset: selection.name.clone(),
                source,
            })?;
        Ok(preset.build(&params))
    }

    /// Every registered preset, ordered by name.
    pub fn descriptors(&self) -> impl Iterator<Item = &PresetDescriptor> {
        self.presets.values().map(|p| p.descriptor())
    }
}

impl std::fmt::Debug for PresetRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.presets.keys()).finish()
    }
}

struct BuiltinPreset {
    descriptor: PresetDescriptor,
    build: fn(&ParamValues) -> GraphSpec,
}

impl PresetFactory for BuiltinPreset {
    fn descriptor(&self) -> &PresetDescriptor {
        &self.descriptor
    }

    fn build(&self, params: &ParamValues) -> GraphSpec {
        (self.build)(params)
    }
}

fn preset<const N: usize>(
    name: &str,
    description: &str,
    params: [ParamSpec; N],
    build: fn(&ParamValues) -> GraphSpec,
) -> BuiltinPreset {
    let params: Vec<ParamSpec> = params.into();
    let graph = build(&params.defaults());
    BuiltinPreset {
        descriptor: PresetDescriptor {
            name: name.into(),
            description: description.into(),
            params,
            graph,
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

/// A node with parameters as `(name, value)`; the others keep their defaults.
fn node(id: &str, kind: &str, params: &[(&str, f32)]) -> NodeSpec {
    NodeSpec {
        id: id.into(),
        kind: kind.into(),
        params: params
            .iter()
            .map(|&(name, value)| (name.to_owned(), Value::from(f64::from(value))))
            .collect(),
    }
}

/// `nodes` one after another from the graph's input to its output.
fn chain(rate: Option<f32>, nodes: Vec<NodeSpec>) -> GraphSpec {
    GraphSpec {
        rate: rate.filter(|&rate| rate != 1.0),
        nodes,
        edges: Vec::new(),
    }
}

fn edge(from: &str, to: &str) -> Edge {
    let endpoint = |node: &str| Endpoint {
        node: node.to_owned(),
        port: None,
    };
    Edge {
        from: endpoint(from),
        to: endpoint(to),
    }
}

/// The linear gain of `db` decibels.
fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Semitones that undo the pitch change of playing at `rate`.
fn semitones_against(rate: f32) -> f32 {
    -12.0 * rate.log2()
}

/// A node that mixes both channels into the middle.
fn mono(id: &str) -> NodeSpec {
    node(
        id,
        "channel_mix",
        &[
            ("left_to_left", 0.5),
            ("right_to_left", 0.5),
            ("left_to_right", 0.5),
            ("right_to_right", 0.5),
        ],
    )
}

// The presets keep about the loudness of what they play: measured on music, each is within
// about 2 LU of the original, apart from what it adds on purpose (bass_boost's bass). The
// `level` nodes make up for what filters and saturation change.
fn builtins() -> Vec<BuiltinPreset> {
    vec![
        preset(
            "bass_boost",
            "More bass, as much as fits without clipping: the bass below the frequency is split off \
             and raised, then compressed and lightly saturated, so that its peaks stay in range and \
             it is heard even on small speakers. The rest is lowered by a quarter of the boost to \
             make room, and a limiter catches what is left.",
            [
                param("gain_db", "Bass boost in dB.", 10.0, 0.0..=18.0),
                param(
                    "frequency",
                    "Everything below this frequency, in Hz, is boosted.",
                    150.0,
                    40.0..=250.0,
                ),
            ],
            |p| {
                let (gain_db, frequency) = (p.get("gain_db"), p.get("frequency"));
                GraphSpec {
                    rate: None,
                    nodes: vec![
                        node("subsonic", "highpass", &[("cutoff", 25.0)]),
                        // two Butterworth filters each way: a crossover whose halves add up flat
                        node("low1", "lowpass", &[("cutoff", frequency)]),
                        node("low2", "lowpass", &[("cutoff", frequency)]),
                        node(
                            "bass",
                            "compressor",
                            &[
                                // only what the boost pushes above -6 dBFS is compressed
                                ("threshold", -6.0 - gain_db),
                                ("ratio", 3.0),
                                ("attack", 0.02),
                                ("release", 0.15),
                                ("makeup", gain_db),
                            ],
                        ),
                        node(
                            "grit",
                            "drive",
                            &[("drive", 2.5), ("mix", (gain_db / 25.0).min(0.5))],
                        ),
                        node("high1", "highpass", &[("cutoff", frequency)]),
                        node("high2", "highpass", &[("cutoff", frequency)]),
                        node("rest", "volume", &[("gain", db_to_gain(-gain_db / 4.0))]),
                        node("limiter", "limiter", &[]),
                    ],
                    edges: vec![
                        edge(GRAPH_INPUT, "subsonic"),
                        edge("subsonic", "low1"),
                        edge("low1", "low2"),
                        edge("low2", "bass"),
                        edge("bass", "grit"),
                        edge("grit", "limiter"),
                        edge("subsonic", "high1"),
                        edge("high1", "high2"),
                        edge("high2", "rest"),
                        edge("rest", "limiter"),
                        edge("limiter", GRAPH_OUTPUT),
                    ],
                }
            },
        ),
        preset(
            "nightcore",
            "Faster and higher.",
            [param("rate", "Playback rate.", 1.25, 1.0..=2.0)],
            |p| chain(Some(p.get("rate")), Vec::new()),
        ),
        preset(
            "sped_up",
            "Faster at the original pitch.",
            [param("rate", "Playback rate.", 1.25, 1.0..=2.0)],
            |p| {
                let rate = p.get("rate");
                chain(
                    Some(rate),
                    vec![node(
                        "pitch",
                        "pitch",
                        &[("semitones", semitones_against(rate))],
                    )],
                )
            },
        ),
        preset(
            "vaporwave",
            "Slower and lower, muffled and gently wobbling, in a large room.",
            [param("rate", "Playback rate.", 0.8, 0.5..=1.0)],
            |p| {
                chain(
                    Some(p.get("rate")),
                    vec![
                        node("muffle", "lowpass", &[("cutoff", 6000.0), ("q", 0.5)]),
                        node("wobble", "vibrato", &[("frequency", 0.3), ("depth", 0.1)]),
                        node(
                            "reverb",
                            "reverb",
                            &[
                                ("room_size", 25.0),
                                ("time", 3.0),
                                ("damping", 0.6),
                                ("mix", 0.35),
                                ("pre_delay", 0.02),
                                ("low_cut", 150.0),
                            ],
                        ),
                    ],
                )
            },
        ),
        preset(
            "slowed_reverb",
            "Slower and lower, drenched in a long, dark reverberation.",
            [
                param("rate", "Playback rate.", 0.85, 0.5..=1.0),
                param("mix", "How much reverb.", 0.5, 0.0..=1.0),
            ],
            |p| {
                chain(
                    Some(p.get("rate")),
                    vec![node(
                        "reverb",
                        "reverb",
                        &[
                            ("room_size", 30.0),
                            ("time", 5.0),
                            ("damping", 0.7),
                            ("mix", p.get("mix")),
                            ("pre_delay", 0.04),
                            ("low_cut", 200.0),
                        ],
                    )],
                )
            },
        ),
        preset(
            "8d",
            "Circles around your head (\"8D audio\") in a small room; best on headphones.",
            [
                param("frequency", "Rounds per second.", 0.15, 0.01..=2.0),
                param(
                    "depth",
                    "How far towards each side it goes.",
                    0.8,
                    0.0..=1.0,
                ),
            ],
            |p| GraphSpec {
                rate: None,
                nodes: vec![
                    node(
                        "orbit",
                        "orbit",
                        &[("frequency", p.get("frequency")), ("depth", p.get("depth"))],
                    ),
                    // the room stays around the listener while the sound moves through it
                    node(
                        "room",
                        "reverb",
                        &[
                            ("room_size", 15.0),
                            ("time", 1.2),
                            ("mix", 1.0),
                            ("pre_delay", 0.02),
                            ("low_cut", 200.0),
                        ],
                    ),
                    node("room_level", "volume", &[("gain", 0.3)]),
                ],
                edges: vec![
                    edge(GRAPH_INPUT, "orbit"),
                    edge("orbit", GRAPH_OUTPUT),
                    edge(GRAPH_INPUT, "room"),
                    edge("room", "room_level"),
                    edge("room_level", GRAPH_OUTPUT),
                ],
            },
        ),
        preset(
            "karaoke",
            "Removes centered vocals, keeping the bass below the band untouched. Works on songs \
             mixed with the vocals in the middle.",
            [param(
                "band",
                "Below this frequency, in Hz, nothing is removed.",
                220.0,
                60.0..=1000.0,
            )],
            |p| {
                let band = p.get("band");
                GraphSpec {
                    rate: None,
                    nodes: vec![
                        node("low", "lowpass", &[("cutoff", band)]),
                        node("high", "highpass", &[("cutoff", band)]),
                        // the difference of the channels in both: what they share cancels
                        node(
                            "cancel",
                            "channel_mix",
                            &[
                                ("left_to_left", 1.0),
                                ("right_to_left", -1.0),
                                ("left_to_right", 1.0),
                                ("right_to_right", -1.0),
                            ],
                        ),
                    ],
                    edges: vec![
                        edge(GRAPH_INPUT, "low"),
                        edge(GRAPH_INPUT, "high"),
                        edge("high", "cancel"),
                        edge("low", GRAPH_OUTPUT),
                        edge("cancel", GRAPH_OUTPUT),
                    ],
                }
            },
        ),
        preset(
            "lofi",
            "Warm, muffled and wobbly, with the hiss and crackle of a worn record.",
            [
                param(
                    "wow",
                    "How far the pitch wobbles, in semitones.",
                    0.15,
                    0.0..=1.0,
                ),
                param(
                    "noise",
                    "How loud the hiss and crackle are.",
                    0.3,
                    0.0..=1.0,
                ),
            ],
            |p| {
                chain(
                    None,
                    vec![
                        node("thin", "highpass", &[("cutoff", 120.0)]),
                        node("muffle", "lowpass", &[("cutoff", 4500.0), ("q", 0.6)]),
                        node(
                            "wow",
                            "vibrato",
                            &[("frequency", 0.5), ("depth", p.get("wow"))],
                        ),
                        node("warmth", "drive", &[("drive", 2.0), ("mix", 0.5)]),
                        node("level", "volume", &[("gain", db_to_gain(-1.5))]),
                        node(
                            "vinyl",
                            "vinyl",
                            &[("amount", p.get("noise")), ("density", 6.0)],
                        ),
                    ],
                )
            },
        ),
        preset(
            "phonograph",
            "An old record player: mono, through a small horn, wobbling and crackling.",
            [
                param(
                    "wow",
                    "How far the pitch wobbles, in semitones.",
                    0.1,
                    0.0..=1.0,
                ),
                param(
                    "noise",
                    "How loud the hiss and crackle are.",
                    0.8,
                    0.0..=1.0,
                ),
            ],
            |p| {
                chain(
                    None,
                    vec![
                        mono("mono"),
                        // resonant edges, like a horn's
                        node("low_cut", "highpass", &[("cutoff", 250.0), ("q", 1.2)]),
                        node("high_cut", "lowpass", &[("cutoff", 5000.0), ("q", 1.2)]),
                        node(
                            "wow",
                            "vibrato",
                            &[("frequency", 0.55), ("depth", p.get("wow"))],
                        ),
                        node("horn", "drive", &[("drive", 2.0), ("mix", 0.6)]),
                        node("level", "volume", &[("gain", db_to_gain(-1.7))]),
                        node(
                            "vinyl",
                            "vinyl",
                            &[("amount", p.get("noise")), ("density", 15.0)],
                        ),
                    ],
                )
            },
        ),
        preset(
            "telephone",
            "Through a phone line: mono, only the voice band, a little distorted.",
            [],
            |_| {
                chain(
                    None,
                    vec![
                        mono("mono"),
                        // two filters each side, for the steep edges of a phone line's band
                        node("low_cut", "highpass", &[("cutoff", 350.0), ("q", 1.0)]),
                        node("low_cut2", "highpass", &[("cutoff", 350.0)]),
                        node("high_cut", "lowpass", &[("cutoff", 3200.0), ("q", 1.0)]),
                        node("high_cut2", "lowpass", &[("cutoff", 3200.0)]),
                        node(
                            "honk",
                            "bell",
                            &[("center", 1500.0), ("q", 1.0), ("gain_db", 3.0)],
                        ),
                        node("line", "drive", &[("drive", 4.0), ("mix", 0.4)]),
                        node("level", "volume", &[("gain", db_to_gain(-2.4))]),
                    ],
                )
            },
        ),
        preset(
            "chipmunk",
            "Higher at the original speed.",
            [param(
                "semitones",
                "Shift up, in semitones.",
                7.0,
                1.0..=12.0,
            )],
            |p| {
                chain(
                    None,
                    vec![node("pitch", "pitch", &[("semitones", p.get("semitones"))])],
                )
            },
        ),
        preset(
            "deep",
            "Lower at the original speed.",
            [param(
                "semitones",
                "Shift down, in semitones.",
                5.0,
                1.0..=12.0,
            )],
            |p| {
                chain(
                    None,
                    vec![node(
                        "pitch",
                        "pitch",
                        &[("semitones", -p.get("semitones"))],
                    )],
                )
            },
        ),
        preset(
            "concert_hall",
            "In a large concert hall.",
            [param("mix", "How much reverb.", 0.4, 0.0..=1.0)],
            |p| {
                chain(
                    None,
                    vec![node(
                        "hall",
                        "reverb",
                        &[
                            ("room_size", 30.0),
                            ("time", 3.0),
                            ("damping", 0.4),
                            ("mix", p.get("mix")),
                            ("pre_delay", 0.03),
                            ("low_cut", 150.0),
                        ],
                    )],
                )
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    use std::f32::consts::TAU;

    use serde_json::json;

    use super::*;
    use crate::dsp::NodeRegistry;

    fn selection(name: &str, params: Value) -> PresetSelection {
        serde_json::from_value(json!({ "name": name, "params": params })).unwrap()
    }

    #[test]
    fn every_preset_compiles_at_its_defaults_and_extremes() {
        let presets = PresetRegistry::with_builtins();
        let nodes = NodeRegistry::with_builtins();
        for descriptor in presets.descriptors() {
            let at = |pick: fn(&ParamSpec) -> f32| -> Map<String, Value> {
                descriptor
                    .params
                    .iter()
                    .map(|p| (p.name.clone(), Value::from(f64::from(pick(p)))))
                    .collect()
            };
            nodes
                .compile(&descriptor.graph, 48_000.0)
                .unwrap_or_else(|e| panic!("{}: {e}", descriptor.name));
            for params in [at(|p| p.min), at(|p| p.max)] {
                let graph = presets
                    .build(&PresetSelection {
                        name: descriptor.name.clone(),
                        params: Some(params),
                    })
                    .unwrap();
                nodes
                    .compile(&graph, 48_000.0)
                    .unwrap_or_else(|e| panic!("{}: {e}", descriptor.name));
            }
        }
    }

    /// The peak, and the amplitudes at 60 and 1000 Hz, of a test mix (a 60 Hz bass under a
    /// 1 kHz tone) through `graph`.
    fn measure(graph: &GraphSpec) -> (f32, f32, f32) {
        let mut effects = NodeRegistry::with_builtins()
            .compile(graph, 48_000.0)
            .unwrap();
        let mut out = vec![0.0; 48_000 * 2];
        let mut t = 0usize;
        effects.render(&mut out, |input| {
            for frame in input.as_chunks_mut::<2>().0 {
                let seconds = t as f32 / 48_000.0;
                let mix =
                    0.45 * (TAU * 60.0 * seconds).sin() + 0.45 * (TAU * 1000.0 * seconds).sin();
                frame.fill(mix);
                t += 1;
            }
        });
        // past the filters' settling, a whole number of cycles of both
        let (frames, _) = out[24_000 * 2..].as_chunks::<2>();
        let peak = frames.iter().fold(0.0f32, |m, f| m.max(f[0].abs()));
        // whatever the phase after the filters; the other tone averages out
        let amplitude = |hz: f32| {
            let (mut in_phase, mut quadrature) = (0.0f32, 0.0f32);
            for (i, frame) in frames.iter().enumerate() {
                let angle = TAU * hz * i as f32 / 48_000.0;
                in_phase += frame[0] * angle.sin();
                quadrature += frame[0] * angle.cos();
            }
            2.0 * in_phase.hypot(quadrature) / frames.len() as f32
        };
        (peak, amplitude(60.0), amplitude(1000.0))
    }

    #[test]
    fn bass_boost_lifts_the_bass_over_the_rest_without_clipping() {
        let presets = PresetRegistry::with_builtins();
        let (_, dry_bass, dry_tone) = measure(&GraphSpec::default());
        let boosted = presets
            .build(&selection("bass_boost", json!({ "gain_db": 12 })))
            .unwrap();
        let (peak, bass, tone) = measure(&boosted);
        assert!(
            peak <= 1.0,
            "the limiter keeps peaks within full scale, got {peak}"
        );
        let lift_db = 20.0 * ((bass / tone) / (dry_bass / dry_tone)).log10();
        // the mix already peaks at 0.9, so the bass's peaks are compressed: not all 12 dB fit
        assert!(
            (7.0..=12.5).contains(&lift_db),
            "the bass rose {lift_db:.1} dB over the 1 kHz tone"
        );
    }

    /// Deterministic noise with the falling spectrum of most music, at about -20 dBFS: pink noise
    /// that falls faster above a few kHz, as mixes do. Some is common to both channels and some
    /// each channel's own, for a mix's stereo image: the sides 6.5 dB under the middle.
    fn music_like() -> impl FnMut() -> [f32; 2] {
        const WIDTH: f32 = 0.75;
        // a one-pole low-pass at 5 kHz
        let smoothing = 1.0 - (-TAU * 5000.0 / 48_000.0).exp();
        let mut state = 0x2545_f491_u32;
        let mut filters = [[0.0f32; 4]; 3];
        move || {
            let mut pink = [0.0; 3];
            for (pink, b) in pink.iter_mut().zip(&mut filters) {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                let white = state as f32 / u32::MAX as f32 * 2.0 - 1.0;
                // Paul Kellet's economy pinking filter
                b[0] = 0.99765 * b[0] + white * 0.099_046;
                b[1] = 0.963 * b[1] + white * 0.296_516_4;
                b[2] = 0.57 * b[2] + white * 1.052_691_3;
                let raw = (b[0] + b[1] + b[2] + white * 0.1848) * 0.05;
                b[3] += (raw - b[3]) * smoothing;
                *pink = b[3];
            }
            [pink[0] + WIDTH * pink[1], pink[0] + WIDTH * pink[2]]
        }
    }

    /// Loudness in dB as meters measure it (BS.1770's K-weighting at 48 kHz, without gating) of
    /// interleaved stereo.
    fn loudness(audio: &[f32]) -> f64 {
        const STAGES: [([f64; 3], [f64; 2]); 2] = [
            (
                [
                    1.535_124_859_586_97,
                    -2.691_696_189_406_38,
                    1.198_392_810_852_85,
                ],
                [-1.690_659_293_182_41, 0.732_480_774_215_85],
            ),
            (
                [1.0, -2.0, 1.0],
                [-1.990_047_454_833_98, 0.990_072_250_366_21],
            ),
        ];
        let mut power = 0.0;
        for channel in 0..2 {
            let mut signal: Vec<f64> = audio
                .iter()
                .skip(channel)
                .step_by(2)
                .map(|&s| f64::from(s))
                .collect();
            for (b, a) in STAGES {
                let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
                for sample in &mut signal {
                    let y = b[0] * *sample + b[1] * x1 + b[2] * x2 - a[0] * y1 - a[1] * y2;
                    (x2, x1, y2, y1) = (x1, *sample, y1, y);
                    *sample = y;
                }
            }
            power += signal.iter().map(|s| s * s).sum::<f64>() / signal.len() as f64;
        }
        10.0 * power.log10()
    }

    #[test]
    fn presets_keep_the_loudness_of_what_they_play() {
        let presets = PresetRegistry::with_builtins();
        let nodes = NodeRegistry::with_builtins();
        let play = |graph: &GraphSpec| {
            let mut effects = nodes.compile(graph, 48_000.0).unwrap();
            let mut music = music_like();
            let mut out = vec![0.0; 48_000 * 2 * 6];
            effects.render(&mut out, |input| {
                for frame in input.as_chunks_mut::<2>().0 {
                    *frame = music();
                }
            });
            // once the reverberations have built up
            loudness(&out[48_000 * 2 * 2..])
        };
        let dry = play(&GraphSpec::default());
        for descriptor in presets.descriptors() {
            let change = play(&descriptor.graph) - dry;
            // noise only approximates music: on songs, they are within about 1.5 dB; bass_boost
            // adds its bass on top
            let allowed = if descriptor.name == "bass_boost" {
                0.0..=8.0
            } else {
                -2.5..=2.5
            };
            assert!(
                allowed.contains(&change),
                "{} changes the loudness by {change:+.1} dB",
                descriptor.name
            );
        }
    }

    #[test]
    fn parameters_shape_the_graph() {
        let presets = PresetRegistry::with_builtins();
        let graph = presets
            .build(&selection("bass_boost", json!({ "gain_db": 12 })))
            .unwrap();
        let node = |id: &str| graph.nodes.iter().find(|n| n.id == id).unwrap();
        assert_eq!(node("bass").params["makeup"], 12.0);
        assert_eq!(node("bass").params["threshold"], -18.0);
        let rest = node("rest").params["gain"].as_f64().unwrap();
        assert!(
            (rest - 0.708).abs() < 0.01,
            "a quarter of the boost: -3 dB, got {rest}"
        );

        let sped_up = presets
            .build(&selection("sped_up", json!({ "rate": 2 })))
            .unwrap();
        assert_eq!(sped_up.rate, Some(2.0));
        assert_eq!(
            sped_up.nodes[0].params["semitones"], -12.0,
            "an octave back down"
        );

        assert!(matches!(
            presets.build(&selection("nightcore", json!({ "rate": 5 }))),
            Err(PresetError::Param { .. })
        ));
        assert!(matches!(
            presets.build(&selection("warp", json!({}))),
            Err(PresetError::Unknown(_))
        ));
    }
}
