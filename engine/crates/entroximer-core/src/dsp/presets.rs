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

fn builtins() -> Vec<BuiltinPreset> {
    vec![
        preset(
            "bass_boost",
            "More bass, the way mastering engineers add it: rumble below hearing is filtered out \
             first so it cannot eat the headroom, a low shelf lifts the bass, a gentle cut around \
             350 Hz keeps the boost from turning muddy, and the level is lowered by half the boost \
             before a limiter catches the remaining peaks, so the limiter does not pump.",
            [
                param(
                    "gain_db",
                    "Bass boost in dB; 6 to 8 is a strong but clean boost.",
                    6.0,
                    0.0..=18.0,
                ),
                param(
                    "frequency",
                    "Everything below this frequency, in Hz, is boosted.",
                    100.0,
                    40.0..=250.0,
                ),
            ],
            |p| {
                let gain_db = p.get("gain_db");
                chain(
                    None,
                    vec![
                        node("subsonic", "highpass", &[("cutoff", 30.0)]),
                        node(
                            "headroom",
                            "volume",
                            &[("gain", db_to_gain(-gain_db / 2.0))],
                        ),
                        node(
                            "bass",
                            "lowshelf",
                            &[("cutoff", p.get("frequency")), ("gain_db", gain_db)],
                        ),
                        node(
                            "mud",
                            "bell",
                            &[("center", 350.0), ("q", 1.0), ("gain_db", -gain_db / 4.0)],
                        ),
                        node("limiter", "limiter", &[]),
                    ],
                )
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
            "Slower and lower, muffled and spacious.",
            [param("rate", "Playback rate.", 0.8, 0.5..=1.0)],
            |p| {
                chain(
                    Some(p.get("rate")),
                    vec![
                        node("muffle", "lowpass", &[("cutoff", 5000.0)]),
                        node(
                            "reverb",
                            "reverb",
                            &[("room_size", 20.0), ("time", 3.0), ("mix", 0.25)],
                        ),
                    ],
                )
            },
        ),
        preset(
            "slowed_reverb",
            "Slower and lower, in a large reverberant room.",
            [
                param("rate", "Playback rate.", 0.85, 0.5..=1.0),
                param("mix", "How much reverb.", 0.35, 0.0..=1.0),
            ],
            |p| {
                chain(
                    Some(p.get("rate")),
                    vec![node(
                        "reverb",
                        "reverb",
                        &[
                            ("room_size", 30.0),
                            ("time", 4.0),
                            ("damping", 0.4),
                            ("mix", p.get("mix")),
                        ],
                    )],
                )
            },
        ),
        preset(
            "8d",
            "Circles from side to side in a small room (\"8D audio\"); best on headphones.",
            [param("frequency", "Rounds per second.", 0.125, 0.01..=2.0)],
            |p| {
                chain(
                    None,
                    vec![
                        node("pan", "auto_pan", &[("frequency", p.get("frequency"))]),
                        node(
                            "room",
                            "reverb",
                            &[("room_size", 15.0), ("time", 1.5), ("mix", 0.15)],
                        ),
                    ],
                )
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
            "Warm, narrow and slightly wobbly, like a worn tape.",
            [param("wow", "How much the pitch wobbles.", 0.15, 0.0..=1.0)],
            |p| {
                chain(
                    None,
                    vec![
                        node("thin", "highpass", &[("cutoff", 150.0)]),
                        node("muffle", "lowpass", &[("cutoff", 3500.0)]),
                        node(
                            "wow",
                            "vibrato",
                            &[("frequency", 0.6), ("depth", p.get("wow"))],
                        ),
                        node("warmth", "drive", &[("drive", 2.0), ("mix", 0.5)]),
                    ],
                )
            },
        ),
        preset(
            "phonograph",
            "An old record player: mono, band-limited, with a slow wobble.",
            [param("wow", "How much the pitch wobbles.", 0.25, 0.0..=1.0)],
            |p| {
                chain(
                    None,
                    vec![
                        node(
                            "mono",
                            "channel_mix",
                            &[
                                ("left_to_left", 0.5),
                                ("right_to_left", 0.5),
                                ("left_to_right", 0.5),
                                ("right_to_right", 0.5),
                            ],
                        ),
                        node("low_cut", "highpass", &[("cutoff", 300.0)]),
                        node("high_cut", "lowpass", &[("cutoff", 4500.0)]),
                        node(
                            "wow",
                            "vibrato",
                            &[("frequency", 0.5), ("depth", p.get("wow"))],
                        ),
                        node("horn", "drive", &[("drive", 1.5), ("mix", 0.6)]),
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
                        node(
                            "mono",
                            "channel_mix",
                            &[
                                ("left_to_left", 0.5),
                                ("right_to_left", 0.5),
                                ("left_to_right", 0.5),
                                ("right_to_right", 0.5),
                            ],
                        ),
                        node("low_cut", "highpass", &[("cutoff", 400.0)]),
                        node("high_cut", "lowpass", &[("cutoff", 3000.0)]),
                        node("line", "drive", &[("drive", 3.0), ("mix", 0.4)]),
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
            "In a large hall.",
            [param("mix", "How much reverb.", 0.35, 0.0..=1.0)],
            |p| {
                chain(
                    None,
                    vec![node(
                        "hall",
                        "reverb",
                        &[
                            ("room_size", 40.0),
                            ("time", 3.5),
                            ("damping", 0.3),
                            ("mix", p.get("mix")),
                        ],
                    )],
                )
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
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
        use std::f32::consts::TAU;

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
        // 60 Hz is below the shelf's 100 Hz midpoint, so it gets most of the 12 dB
        assert!(
            (8.0..=12.5).contains(&lift_db),
            "the bass rose {lift_db:.1} dB over the 1 kHz tone"
        );
    }

    #[test]
    fn parameters_shape_the_graph() {
        let presets = PresetRegistry::with_builtins();
        let graph = presets
            .build(&selection("bass_boost", json!({ "gain_db": 12 })))
            .unwrap();
        let node = |id: &str| graph.nodes.iter().find(|n| n.id == id).unwrap();
        assert_eq!(node("bass").params["gain_db"], 12.0);
        assert_eq!(node("mud").params["gain_db"], -3.0);
        let headroom = node("headroom").params["gain"].as_f64().unwrap();
        assert!(
            (headroom - 0.5).abs() < 0.01,
            "half the boost: -6 dB, got {headroom}"
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
