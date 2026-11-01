//! Graph descriptions sent by clients, and their compilation into fundsp networks.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::str::FromStr;

use fundsp::audiounit::AudioUnit;
use fundsp::net::{Net, NodeId};
use fundsp::prelude32::pass;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::effects::Effects;
use super::rate::{RATE_RANGE, RateStage};
use super::{NodeRegistry, ParamError, ParamSpecs};

pub const GRAPH_INPUT: &str = "input";
pub const GRAPH_OUTPUT: &str = "output";
/// Graphs process interleaved stereo.
pub const CHANNELS: usize = 2;

/// How nodes connect between the graph's own stereo `input` and `output`:
///
/// ```json
/// {
///   "nodes": [
///     { "id": "low", "type": "lowpass", "params": { "cutoff": 400 } },
///     { "id": "verb", "type": "reverb", "params": { "mix": 1 } }
///   ],
///   "edges": [
///     { "from": "input", "to": "low" },
///     { "from": "input", "to": "verb" },
///     { "from": "low", "to": "output" },
///     { "from": "verb:1", "to": "output:1" }
///   ]
/// }
/// ```
///
/// An endpoint without a port connects all ports pairwise. Several sources feeding one input
/// are summed. Without `edges`, the nodes form a chain from `input` to `output` in the order
/// they are listed.
///
/// `rate` (0.5 to 2, default 1) plays what feeds `input` faster and higher or slower and lower,
/// like a turntable at another speed; nightcore is a rate of about 1.25. It is not a node because
/// it changes how much of the tracks is consumed, which nodes (one frame in, one frame out)
/// cannot.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GraphSpec {
    /// Playback rate of what feeds the graph's input, 0.5 to 2; 1 if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub rate: Option<f32>,
    pub nodes: Vec<NodeSpec>,
    /// When empty (or absent), `nodes` form a chain from the graph input to the graph output,
    /// in order.
    #[serde(default)]
    pub edges: Vec<Edge>,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeSpec {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    /// Parameters by name; absent ones take their defaults.
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(type = "Record<string, number>"))]
    pub params: Map<String, Value>,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub from: Endpoint,
    pub to: Endpoint,
}

/// `node` (every port) or `node:port`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(type = "string"))]
pub struct Endpoint {
    pub node: String,
    pub port: Option<usize>,
}

#[derive(Debug, thiserror::Error)]
#[error("invalid endpoint `{0}`, expected `node` or `node:port`")]
pub struct InvalidEndpoint(String);

impl FromStr for Endpoint {
    type Err = InvalidEndpoint;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || InvalidEndpoint(s.to_owned());
        let (node, port) = match s.split_once(':') {
            Some((node, port)) => (node, Some(port.parse().map_err(|_| invalid())?)),
            None => (s, None),
        };
        if node.is_empty() {
            return Err(invalid());
        }
        Ok(Self {
            node: node.to_owned(),
            port,
        })
    }
}

impl TryFrom<String> for Endpoint {
    type Error = InvalidEndpoint;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<Endpoint> for String {
    fn from(e: Endpoint) -> Self {
        e.to_string()
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.port {
            Some(port) => write!(f, "{}:{port}", self.node),
            None => f.write_str(&self.node),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    #[error("the rate must be within {min}..={max}, got {0}", min = RATE_RANGE.start(), max = RATE_RANGE.end())]
    Rate(f32),
    #[error("unknown node type `{0}`")]
    UnknownType(String),
    #[error("node id `{0}` is used more than once")]
    DuplicateId(String),
    #[error("node id `{0}` is reserved for the graph's own ports")]
    ReservedId(String),
    #[error("invalid parameters for node `{node}`")]
    Param {
        node: String,
        #[source]
        source: ParamError,
    },
    #[error("edge refers to unknown node `{0}`")]
    UnknownNode(String),
    #[error("`{endpoint}` does not exist; the node has {available} port(s) on that side")]
    NoSuchPort {
        endpoint: Endpoint,
        available: usize,
    },
    #[error("cannot connect `{from}` ({from_ports} ports) to `{to}` ({to_ports} ports)")]
    PortCountMismatch {
        from: Endpoint,
        from_ports: usize,
        to: Endpoint,
        to_ports: usize,
    },
    #[error("`{0}` cannot be the source of an edge")]
    OutputAsSource(Endpoint),
    #[error("`{0}` cannot be the target of an edge")]
    InputAsTarget(Endpoint),
    #[error("the graph contains a cycle through node `{0}`")]
    Cycle(String),
    #[error("nothing is connected to the graph output")]
    OutputUnconnected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Source {
    GraphInput(usize),
    Node(usize, usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Target {
    GraphOutput(usize),
    Node(usize, usize),
}

struct Placed {
    id: NodeId,
    inputs: usize,
    outputs: usize,
}

impl NodeRegistry {
    /// Validates `spec` and builds its effects, running at `sample_rate`.
    pub fn compile(&self, spec: &GraphSpec, sample_rate: f64) -> Result<Effects, GraphError> {
        let passes_through = spec.nodes.is_empty() && spec.edges.is_empty();
        let network = if passes_through {
            None
        } else {
            Some(Box::new(self.network(spec, sample_rate)?) as Box<dyn AudioUnit>)
        };
        let rate = match spec.rate {
            None | Some(1.0) => None,
            Some(rate) if RATE_RANGE.contains(&rate) => {
                Some(RateStage::new(rate).map_err(|_| GraphError::Rate(rate))?)
            }
            Some(rate) => return Err(GraphError::Rate(rate)),
        };
        Ok(Effects::new(rate, network))
    }

    /// Builds the nodes of `spec` into a stereo network running at `sample_rate`.
    fn network(&self, spec: &GraphSpec, sample_rate: f64) -> Result<Net, GraphError> {
        let mut net = Net::new(CHANNELS, CHANNELS);
        let mut placed = Vec::with_capacity(spec.nodes.len());
        let mut index: HashMap<&str, usize> = HashMap::with_capacity(spec.nodes.len());
        for node in &spec.nodes {
            if node.id == GRAPH_INPUT || node.id == GRAPH_OUTPUT {
                return Err(GraphError::ReservedId(node.id.clone()));
            }
            if index.insert(&node.id, placed.len()).is_some() {
                return Err(GraphError::DuplicateId(node.id.clone()));
            }
            let factory = self
                .get(&node.kind)
                .ok_or_else(|| GraphError::UnknownType(node.kind.clone()))?;
            let params = factory
                .descriptor()
                .params
                .resolve(&node.params)
                .map_err(|source| GraphError::Param {
                    node: node.id.clone(),
                    source,
                })?;
            let unit = factory.build(&params);
            let (inputs, outputs) = (unit.inputs(), unit.outputs());
            placed.push(Placed {
                id: net.push(unit),
                inputs,
                outputs,
            });
        }

        let chained;
        let edges = if spec.edges.is_empty() {
            chained = chain_edges(spec);
            &chained
        } else {
            &spec.edges
        };

        // every target port with the sources feeding it, in a stable order
        let mut wiring: BTreeMap<Target, Vec<Source>> = BTreeMap::new();
        for edge in edges {
            let sources = resolve_sources(&edge.from, &index, &placed)?;
            let targets = resolve_targets(&edge.to, &index, &placed)?;
            if sources.len() != targets.len() {
                return Err(GraphError::PortCountMismatch {
                    from: edge.from.clone(),
                    from_ports: sources.len(),
                    to: edge.to.clone(),
                    to_ports: targets.len(),
                });
            }
            for (source, target) in sources.into_iter().zip(targets) {
                wiring.entry(target).or_default().push(source);
            }
        }
        if !wiring.keys().any(|t| matches!(t, Target::GraphOutput(_))) {
            return Err(GraphError::OutputUnconnected);
        }
        check_acyclic(spec, &wiring)?;

        for (target, sources) in wiring {
            let to = match target {
                Target::GraphOutput(port) => Inlet::GraphOutput(port),
                Target::Node(node, port) => Inlet::Node(placed[node].id, port),
            };
            let outlet = |source: Source| match source {
                Source::GraphInput(port) => Outlet::GraphInput(port),
                Source::Node(node, port) => Outlet::Node(placed[node].id, port),
            };
            let from = match sources.as_slice() {
                [single] => outlet(*single),
                many => {
                    let mixer = net.push(mixer(many.len()));
                    for (port, &source) in many.iter().enumerate() {
                        connect(&mut net, outlet(source), Inlet::Node(mixer, port));
                    }
                    Outlet::Node(mixer, 0)
                }
            };
            connect(&mut net, from, to);
        }

        net.set_sample_rate(sample_rate);
        net.reset();
        Ok(net)
    }
}

/// Where a signal leaves, in network terms.
#[derive(Clone, Copy)]
enum Outlet {
    GraphInput(usize),
    Node(NodeId, usize),
}

/// Where a signal enters, in network terms.
#[derive(Clone, Copy)]
enum Inlet {
    GraphOutput(usize),
    Node(NodeId, usize),
}

fn connect(net: &mut Net, from: Outlet, to: Inlet) {
    match (from, to) {
        (Outlet::GraphInput(i), Inlet::GraphOutput(o)) => net.pass_through(i, o),
        (Outlet::GraphInput(i), Inlet::Node(node, port)) => net.connect_input(i, node, port),
        (Outlet::Node(node, port), Inlet::GraphOutput(o)) => net.connect_output(node, port, o),
        (Outlet::Node(src, sp), Inlet::Node(dst, dp)) => net.connect(src, sp, dst, dp),
    }
}

/// Sums `inputs` signals into one.
fn mixer(inputs: usize) -> Box<dyn AudioUnit> {
    let mut net = Net::new(inputs, 1);
    let mut sum = net.push(Box::new(pass()));
    net.connect_input(0, sum, 0);
    for input in 1..inputs {
        let adder = net.push(Box::new(pass() + pass()));
        net.connect(sum, 0, adder, 0);
        net.connect_input(input, adder, 1);
        sum = adder;
    }
    net.connect_output(sum, 0, 0);
    Box::new(net)
}

fn chain_edges(spec: &GraphSpec) -> Vec<Edge> {
    let endpoint = |node: &str| Endpoint {
        node: node.to_owned(),
        port: None,
    };
    let stops: Vec<&str> = std::iter::once(GRAPH_INPUT)
        .chain(spec.nodes.iter().map(|n| n.id.as_str()))
        .chain(std::iter::once(GRAPH_OUTPUT))
        .collect();
    stops
        .windows(2)
        .map(|pair| Edge {
            from: endpoint(pair[0]),
            to: endpoint(pair[1]),
        })
        .collect()
}

fn ports(endpoint: &Endpoint, available: usize) -> Result<Vec<usize>, GraphError> {
    match endpoint.port {
        Some(port) if port < available => Ok(vec![port]),
        Some(_) => Err(GraphError::NoSuchPort {
            endpoint: endpoint.clone(),
            available,
        }),
        None => Ok((0..available).collect()),
    }
}

fn node_index(endpoint: &Endpoint, index: &HashMap<&str, usize>) -> Result<usize, GraphError> {
    index
        .get(endpoint.node.as_str())
        .copied()
        .ok_or_else(|| GraphError::UnknownNode(endpoint.node.clone()))
}

fn resolve_sources(
    endpoint: &Endpoint,
    index: &HashMap<&str, usize>,
    placed: &[Placed],
) -> Result<Vec<Source>, GraphError> {
    match endpoint.node.as_str() {
        GRAPH_OUTPUT => Err(GraphError::OutputAsSource(endpoint.clone())),
        GRAPH_INPUT => Ok(ports(endpoint, CHANNELS)?
            .into_iter()
            .map(Source::GraphInput)
            .collect()),
        _ => {
            let node = node_index(endpoint, index)?;
            Ok(ports(endpoint, placed[node].outputs)?
                .into_iter()
                .map(|port| Source::Node(node, port))
                .collect())
        }
    }
}

fn resolve_targets(
    endpoint: &Endpoint,
    index: &HashMap<&str, usize>,
    placed: &[Placed],
) -> Result<Vec<Target>, GraphError> {
    match endpoint.node.as_str() {
        GRAPH_INPUT => Err(GraphError::InputAsTarget(endpoint.clone())),
        GRAPH_OUTPUT => Ok(ports(endpoint, CHANNELS)?
            .into_iter()
            .map(Target::GraphOutput)
            .collect()),
        _ => {
            let node = node_index(endpoint, index)?;
            Ok(ports(endpoint, placed[node].inputs)?
                .into_iter()
                .map(|port| Target::Node(node, port))
                .collect())
        }
    }
}

/// fundsp networks must be acyclic; report a cycle as a graph error instead of a panic.
fn check_acyclic(
    spec: &GraphSpec,
    wiring: &BTreeMap<Target, Vec<Source>>,
) -> Result<(), GraphError> {
    let n = spec.nodes.len();
    let mut successors = vec![Vec::new(); n];
    let mut in_degree = vec![0usize; n];
    for (target, sources) in wiring {
        let Target::Node(to, _) = *target else {
            continue;
        };
        for source in sources {
            if let Source::Node(from, _) = *source {
                successors[from].push(to);
                in_degree[to] += 1;
            }
        }
    }
    let mut ready: VecDeque<usize> = (0..n).filter(|&i| in_degree[i] == 0).collect();
    let mut visited = 0;
    while let Some(node) = ready.pop_front() {
        visited += 1;
        for &next in &successors[node] {
            in_degree[next] -= 1;
            if in_degree[next] == 0 {
                ready.push_back(next);
            }
        }
    }
    match (0..n).find(|&i| in_degree[i] > 0) {
        Some(node) if visited < n => Err(GraphError::Cycle(spec.nodes[node].id.clone())),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn spec(value: serde_json::Value) -> GraphSpec {
        serde_json::from_value(value).expect("valid graph json")
    }

    fn run(spec: &GraphSpec, input: [f32; 2]) -> [f32; 2] {
        let mut effects = NodeRegistry::with_builtins()
            .compile(spec, 48_000.0)
            .unwrap();
        let mut out = [0.0; 2];
        effects.render(&mut out, |source| source.copy_from_slice(&input));
        out
    }

    #[test]
    fn empty_graph_passes_through() {
        assert_eq!(run(&GraphSpec::default(), [0.25, -0.5]), [0.25, -0.5]);
    }

    #[test]
    fn chain_without_edges() {
        let g = spec(json!({ "nodes": [
            { "id": "a", "type": "volume", "params": { "gain": 2.0 } },
            { "id": "b", "type": "volume", "params": { "gain": 0.25 } },
        ]}));
        assert_eq!(run(&g, [1.0, -1.0]), [0.5, -0.5]);
    }

    #[test]
    fn fan_in_is_summed_and_ports_route_channels() {
        let g = spec(json!({
            "nodes": [
                { "id": "a", "type": "volume", "params": { "gain": 1.0 } },
                { "id": "b", "type": "volume", "params": { "gain": 2.0 } },
            ],
            "edges": [
                { "from": "input", "to": "a" },
                { "from": "input", "to": "b" },
                { "from": "a", "to": "output" },
                { "from": "b", "to": "output" },
            ]
        }));
        // each output channel receives a + b = 1x + 2x its input
        assert_eq!(run(&g, [1.0, 0.5]), [3.0, 1.5]);

        let swap = spec(json!({
            "nodes": [],
            "edges": [
                { "from": "input:0", "to": "output:1" },
                { "from": "input:1", "to": "output:0" },
            ]
        }));
        assert_eq!(run(&swap, [1.0, 2.0]), [2.0, 1.0]);
    }

    #[test]
    fn invalid_graphs_are_rejected() {
        let registry = NodeRegistry::with_builtins();
        let compile = |v| registry.compile(&spec(v), 48_000.0).err();

        assert!(matches!(
            compile(json!({ "nodes": [{ "id": "a", "type": "nope" }] })),
            Some(GraphError::UnknownType(_))
        ));
        assert!(matches!(
            compile(
                json!({ "nodes": [{ "id": "a", "type": "volume", "params": { "gain": 99 } }] })
            ),
            Some(GraphError::Param {
                source: ParamError::OutOfRange { .. },
                ..
            })
        ));
        assert!(matches!(
            compile(json!({
                "nodes": [{ "id": "a", "type": "volume" }, { "id": "b", "type": "volume" }],
                "edges": [
                    { "from": "a", "to": "b" }, { "from": "b", "to": "a" },
                    { "from": "b", "to": "output" },
                ]
            })),
            Some(GraphError::Cycle(_))
        ));
        assert!(matches!(
            compile(
                json!({ "nodes": [{ "id": "a", "type": "volume" }], "edges": [{ "from": "input", "to": "a" }] })
            ),
            Some(GraphError::OutputUnconnected)
        ));
        assert!(matches!(
            compile(json!({ "nodes": [], "edges": [{ "from": "input:2", "to": "output" }] })),
            Some(GraphError::NoSuchPort { .. })
        ));
        assert!(
            serde_json::from_value::<GraphSpec>(
                json!({ "nodes": [], "edges": [{ "from": "a:x", "to": "output" }] })
            )
            .is_err()
        );
        assert!(matches!(
            compile(json!({ "rate": 3.0, "nodes": [] })),
            Some(GraphError::Rate(_))
        ));
    }

    #[test]
    fn rates_round_trip_and_stay_out_of_the_json_at_one() {
        let sped_up = spec(json!({ "rate": 1.25, "nodes": [] }));
        assert_eq!(sped_up.rate, Some(1.25));
        let effects = NodeRegistry::with_builtins()
            .compile(&sped_up, 48_000.0)
            .unwrap();
        assert_eq!(effects.rate(), 1.25);
        assert_eq!(
            serde_json::to_value(GraphSpec::default()).unwrap(),
            json!({ "nodes": [], "edges": [] })
        );
    }
}
