//! Post-processing: node types, graph descriptions, presets, and their compilation into
//! [`Effects`].
//!
//! - Node types come from a [`NodeRegistry`]; built-ins are registered by
//!   [`NodeRegistry::with_builtins`] and plugins add their own through [`NodeRegistry::register`].
//! - A [`GraphSpec`] wires nodes together and may set a playback rate;
//!   [`NodeRegistry::compile`] turns it into [`Effects`].
//! - Presets ([`PresetRegistry`]) build ready-made graphs, such as nightcore or karaoke, that
//!   clients use as they are or extend with nodes of their own.
//! - A [`Strip`] runs a deck's audio through its effects and volume; [`Ramp`]s change gains
//!   without clicks.

use std::collections::BTreeMap;
use std::sync::Arc;

use fundsp::audiounit::AudioUnit;
use serde::Serialize;
use serde_json::{Map, Value};

mod builtin;
mod chain;
mod effects;
mod graph;
mod nodes;
mod presets;
mod ramp;
mod rate;
mod strip;

pub use chain::{ChainError, parse_chain};
pub use effects::Effects;
pub use graph::{
    CHANNELS, Edge, Endpoint, GRAPH_INPUT, GRAPH_OUTPUT, GraphError, GraphSpec, MAX_EDGES,
    MAX_NODES, NodeSpec,
};
pub use presets::{PresetDescriptor, PresetError, PresetFactory, PresetRegistry, PresetSelection};
pub use ramp::{Curve, Ramp};
pub use rate::RATE_RANGE;
pub use strip::Strip;

/// A node type that graphs can instantiate.
pub trait NodeFactory: Send + Sync {
    fn descriptor(&self) -> &NodeDescriptor;

    /// Builds an instance; `params` has been validated against `descriptor().params`.
    fn build(&self, params: &ParamValues) -> Box<dyn AudioUnit>;
}

/// Public description of a node type, e.g. for clients building graphs.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NodeDescriptor {
    #[serde(rename = "type")]
    pub kind: String,
    pub description: String,
    pub params: Vec<ParamSpec>,
}

/// A numeric parameter of a node type or a preset.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ParamSpec {
    pub name: String,
    pub description: String,
    pub default: f32,
    pub min: f32,
    pub max: f32,
}

#[derive(Debug, thiserror::Error)]
pub enum ParamError {
    #[error("unknown parameter `{0}`")]
    Unknown(String),
    #[error("parameter `{0}` must be a number")]
    NotANumber(String),
    #[error("parameter `{name}` must be within {min}..={max}, got {value}")]
    OutOfRange {
        name: String,
        value: f32,
        min: f32,
        max: f32,
    },
    #[error("expected at most {max} parameters, got {given}")]
    TooMany { max: usize, given: usize },
}

/// Validated parameter values with defaults filled in.
#[derive(Clone, Debug, PartialEq)]
pub struct ParamValues(Vec<(String, f32)>);

impl ParamValues {
    /// # Panics
    /// If `name` is not a parameter declared by the node's or preset's descriptor.
    pub fn get(&self, name: &str) -> f32 {
        self.0
            .iter()
            .find(|(n, _)| n == name)
            .map(|&(_, v)| v)
            .unwrap_or_else(|| panic!("read undeclared parameter `{name}`"))
    }
}

/// Validation of given parameters against their declarations, shared by nodes and presets.
pub trait ParamSpecs {
    /// Validates named parameters, filling in defaults for missing ones.
    fn resolve(&self, given: &Map<String, Value>) -> Result<ParamValues, ParamError>;

    /// Validates parameters given in declaration order, filling in defaults for the rest.
    fn resolve_positional(&self, given: &[f32]) -> Result<ParamValues, ParamError>;

    /// Every parameter at its default.
    fn defaults(&self) -> ParamValues {
        self.resolve_positional(&[])
            .expect("defaults lie within their ranges")
    }
}

impl ParamSpecs for [ParamSpec] {
    fn resolve(&self, given: &Map<String, Value>) -> Result<ParamValues, ParamError> {
        if let Some(unknown) = given.keys().find(|k| !self.iter().any(|p| &p.name == *k)) {
            return Err(ParamError::Unknown(unknown.clone()));
        }
        self.iter()
            .map(|spec| {
                let value = match given.get(&spec.name) {
                    None => spec.default,
                    Some(v) => v
                        .as_f64()
                        .ok_or_else(|| ParamError::NotANumber(spec.name.clone()))?
                        as f32,
                };
                spec.check(value)
            })
            .collect::<Result<_, _>>()
            .map(ParamValues)
    }

    fn resolve_positional(&self, given: &[f32]) -> Result<ParamValues, ParamError> {
        if given.len() > self.len() {
            return Err(ParamError::TooMany {
                max: self.len(),
                given: given.len(),
            });
        }
        self.iter()
            .enumerate()
            .map(|(i, spec)| spec.check(given.get(i).copied().unwrap_or(spec.default)))
            .collect::<Result<_, _>>()
            .map(ParamValues)
    }
}

impl ParamSpec {
    fn check(&self, value: f32) -> Result<(String, f32), ParamError> {
        if (self.min..=self.max).contains(&value) {
            Ok((self.name.clone(), value))
        } else {
            Err(ParamError::OutOfRange {
                name: self.name.clone(),
                value,
                min: self.min,
                max: self.max,
            })
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("node type `{0}` is already registered")]
pub struct DuplicateNodeType(pub String);

#[derive(Default)]
pub struct NodeRegistry {
    factories: BTreeMap<String, Arc<dyn NodeFactory>>,
}

impl NodeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        builtin::register(&mut registry);
        registry
    }

    pub fn register(&mut self, factory: Arc<dyn NodeFactory>) -> Result<(), DuplicateNodeType> {
        let kind = factory.descriptor().kind.clone();
        if self.factories.contains_key(&kind) {
            return Err(DuplicateNodeType(kind));
        }
        self.factories.insert(kind, factory);
        Ok(())
    }

    pub fn get(&self, kind: &str) -> Option<&dyn NodeFactory> {
        self.factories.get(kind).map(|f| f.as_ref())
    }

    /// Every registered node type, ordered by name.
    pub fn descriptors(&self) -> impl Iterator<Item = &NodeDescriptor> {
        self.factories.values().map(|f| f.descriptor())
    }
}

impl std::fmt::Debug for NodeRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.factories.keys()).finish()
    }
}
