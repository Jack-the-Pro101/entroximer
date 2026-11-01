//! A compact text form for linear chains, e.g. `lowpass(800) => reverb(10, 2, 0.5)`.
//!
//! Each step is a node type with optional positional parameters in declaration order; missing
//! parameters take their defaults. An empty string is an empty (pass-through) chain.

use serde_json::{Map, Value};

use super::{GraphSpec, NodeRegistry, NodeSpec, ParamError};

#[derive(Debug, thiserror::Error)]
pub enum ChainError {
    #[error("malformed step `{0}`, expected `type` or `type(1, 2, ...)`")]
    Syntax(String),
    #[error("unknown node type `{0}`")]
    UnknownType(String),
    #[error("`{0}` is not a number")]
    NotANumber(String),
    #[error("invalid parameters for `{kind}`")]
    Param {
        kind: String,
        #[source]
        source: ParamError,
    },
}

pub fn parse_chain(registry: &NodeRegistry, text: &str) -> Result<GraphSpec, ChainError> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(GraphSpec::default());
    }
    let nodes = text
        .split("=>")
        .enumerate()
        .map(|(i, step)| parse_step(registry, i, step.trim()))
        .collect::<Result<_, _>>()?;
    Ok(GraphSpec {
        nodes,
        edges: Vec::new(),
    })
}

fn parse_step(registry: &NodeRegistry, index: usize, step: &str) -> Result<NodeSpec, ChainError> {
    let syntax = || ChainError::Syntax(step.to_owned());
    let (kind, args) = match step.split_once('(') {
        Some((kind, rest)) => (
            kind.trim(),
            Some(rest.strip_suffix(')').ok_or_else(syntax)?),
        ),
        None => (step, None),
    };
    if kind.is_empty() || !kind.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(syntax());
    }
    let values: Vec<f32> = args
        .map(|args| args.trim())
        .filter(|args| !args.is_empty())
        .map(|args| {
            args.split(',')
                .map(|v| {
                    let v = v.trim();
                    v.parse().map_err(|_| ChainError::NotANumber(v.to_owned()))
                })
                .collect::<Result<_, _>>()
        })
        .transpose()?
        .unwrap_or_default();

    let descriptor = registry
        .get(kind)
        .ok_or_else(|| ChainError::UnknownType(kind.to_owned()))?
        .descriptor();
    // validate now so errors point at the step the user wrote
    descriptor
        .resolve_positional(&values)
        .map_err(|source| ChainError::Param {
            kind: kind.to_owned(),
            source,
        })?;
    let params: Map<String, Value> = descriptor
        .params
        .iter()
        .zip(&values)
        .map(|(spec, &value)| (spec.name.clone(), Value::from(f64::from(value))))
        .collect();
    Ok(NodeSpec {
        id: format!("{kind}{index}"),
        kind: kind.to_owned(),
        params,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_steps_with_and_without_parameters() {
        let registry = NodeRegistry::with_builtins();
        let spec = parse_chain(&registry, " lowpass(800) => reverb => volume(0.5) ").unwrap();
        let kinds: Vec<_> = spec.nodes.iter().map(|n| n.kind.as_str()).collect();
        assert_eq!(kinds, ["lowpass", "reverb", "volume"]);
        assert_eq!(spec.nodes[0].params["cutoff"], 800.0);
        assert!(
            !spec.nodes[0].params.contains_key("q"),
            "defaults stay implicit"
        );
        assert!(spec.nodes[1].params.is_empty());
        registry.compile(&spec, 48_000.0).unwrap();
    }

    #[test]
    fn reports_bad_steps() {
        let registry = NodeRegistry::with_builtins();
        assert!(matches!(
            parse_chain(&registry, "volume(1"),
            Err(ChainError::Syntax(_))
        ));
        assert!(matches!(
            parse_chain(&registry, "warp(1)"),
            Err(ChainError::UnknownType(_))
        ));
        assert!(matches!(
            parse_chain(&registry, "volume(x)"),
            Err(ChainError::NotANumber(_))
        ));
        assert!(matches!(
            parse_chain(&registry, "volume(1, 2)"),
            Err(ChainError::Param {
                source: ParamError::TooMany { .. },
                ..
            })
        ));
        assert!(parse_chain(&registry, "").unwrap().nodes.is_empty());
    }
}
