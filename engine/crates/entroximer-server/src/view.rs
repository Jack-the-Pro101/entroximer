//! A player's signal flow laid out for drawing, as `player.view` answers it: each layer's
//! source, rate, effects and volume, joining in the mix. See [`PlayerView`].

mod lanes;

use std::collections::HashMap;

use entroximer_core::dsp::{GRAPH_INPUT, GRAPH_OUTPUT, GraphSpec, NodeSpec};
use entroximer_core::mixer::MAIN;

use crate::api::protocol::{
    ClipMode, ClipStatus, LayerInfo, Millis, PlayerInfo, PlayerView, ViewNode, ViewNodeKind,
    ViewRow,
};

/// Lays out `info`'s signal flow, with the graphs of the layers in `collapse` as one row each.
pub fn player_view(info: &PlayerInfo, collapse: &[String]) -> PlayerView {
    let mut ducked_by: HashMap<&str, Vec<&str>> = HashMap::new();
    for layer in &info.layers {
        for ducked in layer.duck.iter().flat_map(|duck| &duck.layers) {
            ducked_by
                .entry(ducked.as_str())
                .or_default()
                .push(layer.name.as_str());
        }
    }
    let collapsed = |layer: &str| collapse.iter().any(|name| name == layer);

    let mut flow = Flow::default();
    let mut volumes = vec![flow.layer(Strip {
        name: MAIN,
        source: music_source(info),
        graph: info.graph.as_ref(),
        collapsed: collapsed(MAIN),
        volume: info.volume,
        clipping: info.clipping,
        ducked_by: ducked_by.remove(MAIN).unwrap_or_default(),
    })];
    for layer in &info.layers {
        volumes.push(flow.layer(Strip {
            name: &layer.name,
            source: layer_source(layer),
            graph: layer.graph.as_ref(),
            collapsed: collapsed(&layer.name),
            volume: layer.volume,
            clipping: layer.clipping,
            ducked_by: ducked_by.remove(layer.name.as_str()).unwrap_or_default(),
        }));
    }
    let mix = flow.push(
        ViewNodeKind::Mix,
        None,
        "mix".to_owned(),
        vec!["limiter".to_owned()],
    );
    for volume in volumes {
        flow.connect(volume, mix);
    }
    flow.lay_out(info)
}

/// What a layer's audio goes through, from its source to its volume.
struct Strip<'a> {
    name: &'a str,
    /// The source's label and details.
    source: (String, Vec<String>),
    graph: Option<&'a GraphSpec>,
    collapsed: bool,
    volume: u16,
    clipping: bool,
    ducked_by: Vec<&'a str>,
}

/// The nodes of a view, in the order they are listed, and the edges between them.
#[derive(Default)]
struct Flow {
    nodes: Vec<ViewNode>,
    edges: Vec<(usize, usize)>,
}

impl Flow {
    fn push(
        &mut self,
        kind: ViewNodeKind,
        layer: Option<&str>,
        label: String,
        detail: Vec<String>,
    ) -> usize {
        self.nodes.push(ViewNode {
            kind,
            layer: layer.map(str::to_owned),
            id: None,
            graph_tag: None,
            label,
            detail: (!detail.is_empty()).then(|| detail.join(" · ")),
            inputs: Vec::new(),
        });
        self.nodes.len() - 1
    }

    fn connect(&mut self, from: usize, to: usize) {
        self.edges.push((from, to));
    }

    /// Adds a layer's nodes; returns its volume's, which feeds the mix.
    fn layer(&mut self, strip: Strip) -> usize {
        let layer = Some(strip.name);
        let (label, detail) = strip.source;
        let mut last = self.push(ViewNodeKind::Source, layer, label, detail);
        if let Some(rate) = strip
            .graph
            .and_then(|graph| graph.rate)
            .filter(|&rate| rate != 1.0)
        {
            let label = format!("rate {}×", number(f64::from(rate)));
            let node = self.push(ViewNodeKind::Rate, layer, label, Vec::new());
            self.connect(last, node);
            last = node;
        }

        // what feeds the volume: the graph's output, or what would feed the graph
        let mut into_volume = Vec::new();
        match strip.graph.filter(|graph| !graph.nodes.is_empty()) {
            None => into_volume.push(last),
            Some(graph) if strip.collapsed => {
                let label = plural(graph.nodes.len(), "effect");
                let mut kinds: Vec<String> = Vec::new();
                for node in &graph.nodes {
                    if !kinds.contains(&node.kind) {
                        kinds.push(node.kind.clone());
                    }
                }
                let node = self.push(ViewNodeKind::Graph, layer, label, Vec::new());
                self.nodes[node].detail = Some(kinds.join(", "));
                self.nodes[node].graph_tag = Some(graph.tag());
                self.connect(last, node);
                into_volume.push(node);
            }
            Some(graph) => {
                let tag = graph.tag();
                let first = self.nodes.len();
                for spec in &graph.nodes {
                    let node =
                        self.push(ViewNodeKind::Effect, layer, spec.kind.clone(), params(spec));
                    self.nodes[node].id = Some(spec.id.clone());
                    self.nodes[node].graph_tag = Some(tag.clone());
                }
                let node_of = |id: &str| {
                    graph
                        .nodes
                        .iter()
                        .position(|spec| spec.id == id)
                        .map(|index| first + index)
                };
                for edge in graph.wiring().iter() {
                    let from = match edge.from.node.as_str() {
                        GRAPH_INPUT => Some(last),
                        id => node_of(id),
                    };
                    let Some(from) = from else { continue };
                    match edge.to.node.as_str() {
                        GRAPH_OUTPUT => into_volume.push(from),
                        id => {
                            if let Some(to) = node_of(id) {
                                self.connect(from, to);
                            }
                        }
                    }
                }
            }
        }

        let mut detail = Vec::new();
        if strip.clipping {
            detail.push("clipping".to_owned());
        }
        if !strip.ducked_by.is_empty() {
            detail.push(format!("ducked by {}", strip.ducked_by.join(", ")));
        }
        let label = format!("volume {}%", strip.volume);
        let volume = self.push(ViewNodeKind::Volume, layer, label, detail);
        for from in into_volume {
            self.connect(from, volume);
        }
        volume
    }

    fn lay_out(mut self, info: &PlayerInfo) -> PlayerView {
        let glyphs: Vec<char> = self.nodes.iter().map(|node| glyph(node.kind)).collect();
        let layout = lanes::lay_out(&glyphs, &self.edges);
        let mut row_of = vec![0u16; self.nodes.len()];
        for (row, drawn) in layout.rows.iter().enumerate() {
            if let Some(node) = drawn.node {
                row_of[node] = u16::try_from(row).unwrap_or(u16::MAX);
            }
        }
        for &(from, to) in &self.edges {
            self.nodes[to].inputs.push(row_of[from]);
        }
        let mut nodes: Vec<Option<ViewNode>> = self.nodes.into_iter().map(Some).collect();
        let rows = layout
            .rows
            .into_iter()
            .map(|row| ViewRow {
                gutter: row.gutter,
                node: row
                    .node
                    .and_then(|node| nodes[node].take())
                    .map(|mut node| {
                        node.inputs.sort_unstable();
                        node.inputs.dedup();
                        node
                    }),
            })
            .collect();
        PlayerView {
            guild_id: info.guild_id,
            lanes: u8::try_from(layout.lanes).unwrap_or(u8::MAX),
            rows,
        }
    }
}

fn glyph(kind: ViewNodeKind) -> char {
    match kind {
        ViewNodeKind::Source => '■',
        ViewNodeKind::Effect => '●',
        ViewNodeKind::Graph => '◊',
        ViewNodeKind::Rate | ViewNodeKind::Volume => '○',
        ViewNodeKind::Mix => '□',
    }
}

/// The music's current track, and how the queue plays.
fn music_source(info: &PlayerInfo) -> (String, Vec<String>) {
    let label = info.track.as_ref().map_or_else(
        || "nothing playing".to_owned(),
        |track| track.info.title.clone(),
    );
    let mut detail = Vec::new();
    if info.paused {
        detail.push("paused".to_owned());
    }
    if info.queue.length > 0 {
        detail.push(format!("{} queued", info.queue.length));
    }
    if info.queue.crossfade > Millis(0) {
        detail.push(format!("crossfade {}", seconds(info.queue.crossfade)));
    }
    (label, detail)
}

/// A layer's clips, how new ones join them, and whom they duck.
fn layer_source(layer: &LayerInfo) -> (String, Vec<String>) {
    let playing = layer
        .clips
        .iter()
        .filter(|clip| clip.status == ClipStatus::Playing)
        .count();
    let waiting = layer.clips.len() - playing;
    let label = match (playing, waiting) {
        (0, 0) => "no clips".to_owned(),
        (playing, 0) => format!("{} playing", plural(playing, "clip")),
        (0, waiting) => format!("{} waiting", plural(waiting, "clip")),
        (playing, waiting) => format!("{} playing, {waiting} waiting", plural(playing, "clip")),
    };
    let mut detail = vec![match layer.mode {
        ClipMode::Queue => "one at a time".to_owned(),
        ClipMode::Mix => format!("up to {} at once", layer.max_clips),
        ClipMode::Replace => "each replaces the last".to_owned(),
    }];
    if layer.paused {
        detail.push("paused".to_owned());
    }
    if let Some(duck) = &layer.duck {
        detail.push(format!(
            "ducks {} to {}%",
            duck.layers.join(", "),
            duck.volume
        ));
    }
    (label, detail)
}

/// The parameters a node was given, e.g. `cutoff 220 · q 0.5`; defaults are left out, as in
/// the graph.
fn params(spec: &NodeSpec) -> Vec<String> {
    spec.params
        .iter()
        .filter_map(|(name, value)| Some(format!("{name} {}", number(value.as_f64()?))))
        .collect()
}

/// A parameter as people read it: whole from 100 on, otherwise to two decimals at most.
fn number(value: f64) -> String {
    let rounded = if value.abs() >= 100.0 {
        value.round()
    } else {
        (value * 100.0).round() / 100.0
    };
    // no `-0`
    format!("{}", rounded + 0.0)
}

fn seconds(millis: Millis) -> String {
    format!("{} s", number(millis.0 as f64 / 1000.0))
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::api::protocol::{Duck, PlayerState, QueueState, Snowflake};
    use crate::node::queue::Repeat;

    fn info(graph: Option<GraphSpec>, layers: Vec<LayerInfo>) -> PlayerInfo {
        PlayerInfo {
            guild_id: Snowflake(1),
            track: None,
            item_id: None,
            queue: QueueState {
                version: 0,
                length: 0,
                repeat: Repeat::Off,
                crossfade: Millis(0),
            },
            paused: false,
            volume: 80,
            clipping: false,
            graph,
            state: PlayerState {
                time: Millis(0),
                position: Millis(0),
                connected: false,
                buffering: false,
                buffered: Vec::new(),
            },
            layers,
        }
    }

    fn layer(name: &str, duck: Option<Duck>) -> LayerInfo {
        LayerInfo {
            name: name.to_owned(),
            mode: ClipMode::Mix,
            max_clips: 8,
            volume: 100,
            clipping: false,
            paused: false,
            graph: None,
            duck,
            clips: Vec::new(),
        }
    }

    fn graph(value: serde_json::Value) -> GraphSpec {
        serde_json::from_value(value).expect("valid graph json")
    }

    /// Each row as a client might draw it: the gutter, then the node's id or label.
    fn drawn(view: &PlayerView) -> Vec<String> {
        view.rows
            .iter()
            .map(|row| {
                let text = row
                    .node
                    .as_ref()
                    .map_or("", |node| node.id.as_deref().unwrap_or(&node.label));
                format!("{} {text}", row.gutter).trim_end().to_owned()
            })
            .collect()
    }

    fn karaoke() -> GraphSpec {
        graph(json!({
            "nodes": [
                { "id": "low", "type": "lowpass", "params": { "cutoff": 220 } },
                { "id": "high", "type": "highpass", "params": { "cutoff": 220 } },
                { "id": "cancel", "type": "channel_mix" },
            ],
            "edges": [
                { "from": "input", "to": "low" },
                { "from": "input", "to": "high" },
                { "from": "high", "to": "cancel" },
                { "from": "low", "to": "output" },
                { "from": "cancel", "to": "output" },
            ]
        }))
    }

    #[test]
    fn a_graph_branches_inside_its_layer() {
        let view = player_view(&info(Some(karaoke()), Vec::new()), &[]);
        assert_eq!(view.lanes, 2);
        assert_eq!(
            drawn(&view),
            [
                "■   nothing playing",
                "├─┐",
                "● │ low",
                "│ ● high",
                "│ ● cancel",
                "├─┘",
                "○   volume 80%",
                "□   mix",
            ]
        );
    }

    #[test]
    fn effects_are_named_by_layer_id_and_graph_tag() {
        let karaoke = karaoke();
        let view = player_view(&info(Some(karaoke.clone()), Vec::new()), &[]);
        let low = view
            .rows
            .iter()
            .filter_map(|row| row.node.as_ref())
            .find(|node| node.id.as_deref() == Some("low"))
            .unwrap();
        assert_eq!(low.kind, ViewNodeKind::Effect);
        assert_eq!(low.layer.as_deref(), Some(MAIN));
        assert_eq!(low.graph_tag.as_deref(), Some(karaoke.tag().as_str()));
        assert_eq!(low.label, "lowpass");
        assert_eq!(low.detail.as_deref(), Some("cutoff 220"));
        assert_eq!(low.inputs, [0], "fed by the source, on the first row");
        let volume = view.rows[6].node.as_ref().unwrap();
        assert_eq!(volume.inputs, [2, 4], "fed by `low` and `cancel`");
    }

    #[test]
    fn layers_join_in_the_mix() {
        let chain = graph(json!({ "rate": 1.25, "nodes": [{ "id": "verb", "type": "reverb" }] }));
        let duck = Duck {
            layers: vec![MAIN.to_owned()],
            volume: 25,
            attack: None,
            release: None,
            hold: None,
        };
        let view = player_view(
            &info(
                Some(chain),
                vec![layer("sfx", None), layer("voice", Some(duck))],
            ),
            &[],
        );
        assert_eq!(
            drawn(&view),
            [
                "■     nothing playing",
                "○     rate 1.25×",
                "●     verb",
                "○     volume 80%",
                "│ ■   no clips",
                "│ ○   volume 100%",
                "│ │ ■ no clips",
                "│ │ ○ volume 100%",
                "├─┴─┘",
                "□     mix",
            ]
        );
        let detail = |row: usize| view.rows[row].node.as_ref().unwrap().detail.clone();
        assert_eq!(detail(3).as_deref(), Some("ducked by voice"));
        assert_eq!(
            detail(6).as_deref(),
            Some("up to 8 at once · ducks main to 25%")
        );
        assert_eq!(view.rows[9].node.as_ref().unwrap().layer, None);
    }

    #[test]
    fn collapsed_graphs_are_one_row() {
        let view = player_view(&info(Some(karaoke()), Vec::new()), &[MAIN.to_owned()]);
        assert_eq!(
            drawn(&view),
            ["■ nothing playing", "◊ 3 effects", "○ volume 80%", "□ mix"]
        );
        let graph = view.rows[1].node.as_ref().unwrap();
        assert_eq!(graph.kind, ViewNodeKind::Graph);
        assert_eq!(graph.id, None);
        assert_eq!(
            graph.detail.as_deref(),
            Some("lowpass, highpass, channel_mix")
        );
        assert_eq!(graph.graph_tag, Some(karaoke().tag()));
    }

    #[test]
    fn a_dry_path_is_a_wire_without_nodes() {
        let wet = graph(json!({
            "nodes": [{ "id": "verb", "type": "reverb" }],
            "edges": [
                { "from": "input", "to": "verb" },
                { "from": "input", "to": "output" },
                { "from": "verb", "to": "output" },
            ]
        }));
        let view = player_view(&info(Some(wet), Vec::new()), &[]);
        assert_eq!(
            drawn(&view),
            [
                "■   nothing playing",
                "├─┐",
                "● │ verb",
                "├─┘",
                "○   volume 80%",
                "□   mix"
            ]
        );
    }

    #[test]
    fn numbers_read_well() {
        assert_eq!(number(220.0), "220");
        assert_eq!(number(12_345.6), "12346");
        assert_eq!(number(0.707), "0.71");
        assert_eq!(number(-0.001), "0");
        assert_eq!(number(1.25), "1.25");
        assert_eq!(seconds(Millis(2500)), "2.5 s");
    }
}
