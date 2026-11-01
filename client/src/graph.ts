import { EntroximerError } from "./errors";
import type { Edge } from "./generated/Edge";
import type { GraphSpec } from "./generated/GraphSpec";
import type { NodeDescriptor } from "./generated/NodeDescriptor";
import type { NodeSpec } from "./generated/NodeSpec";

/** One effect: a node type (see `NodeInfo.nodes`) and named parameters. */
export interface Effect {
  type: string;
  /** Parameters by name; missing ones take their defaults. */
  params?: Record<string, number>;
  /** The node's id in the graph, to change it later; `type` and a number if absent. */
  id?: string;
}

/** The graph's own ports, which edges start and end at. */
const INPUT = "input";
const OUTPUT = "output";

/** A graph running `effects` one after another. */
export function chain(...effects: Effect[]): GraphSpec {
  return append(null, ...effects);
}

/**
 * `graph` (or an empty one) with `effects` added at its end, one after another: whatever fed the
 * output feeds the first of them, and the last one feeds the output. Builds on presets, e.g.
 * `append(await engine.buildPreset("nightcore"), { type: "reverb" })`.
 */
export function append(graph: GraphSpec | null, ...effects: Effect[]): GraphSpec {
  const base = graph ?? { nodes: [], edges: [] };
  const added = nodesFor(base, effects);
  if (added.length === 0) {
    return base;
  }
  const nodes = [...base.nodes, ...added];
  if (base.edges.length === 0) {
    // without edges, the nodes are a chain in their order
    return { ...base, nodes, edges: [] };
  }
  const first = added[0]!.id;
  const last = added[added.length - 1]!.id;
  const edges = [
    ...base.edges.map((edge) => ({ ...edge, to: renamePort(edge.to, OUTPUT, first) })),
    ...links(added),
    { from: last, to: OUTPUT },
  ];
  return { ...base, nodes, edges };
}

/**
 * `graph` (or an empty one) with `effects` added at its start: the input feeds the first of them,
 * and the last one feeds whatever the input fed.
 */
export function prepend(graph: GraphSpec | null, ...effects: Effect[]): GraphSpec {
  const base = graph ?? { nodes: [], edges: [] };
  const added = nodesFor(base, effects);
  if (added.length === 0) {
    return base;
  }
  const nodes = [...added, ...base.nodes];
  if (base.edges.length === 0) {
    return { ...base, nodes, edges: [] };
  }
  const first = added[0]!.id;
  const last = added[added.length - 1]!.id;
  const edges = [
    { from: INPUT, to: first },
    ...links(added),
    ...base.edges.map((edge) => ({ ...edge, from: renamePort(edge.from, INPUT, last) })),
  ];
  return { ...base, nodes, edges };
}

/** `graph` with the parameters of node `id` changed; the ones not given stay as they are. */
export function withParams(graph: GraphSpec, id: string, params: Record<string, number>): GraphSpec {
  if (!graph.nodes.some((node) => node.id === id)) {
    const known = graph.nodes.map((node) => node.id).join(", ");
    throw badRequest(`the graph has no node \`${id}\`; it has: ${known || "none"}`);
  }
  return {
    ...graph,
    nodes: graph.nodes.map((node) => (node.id === id ? { ...node, params: { ...node.params, ...params } } : node)),
  };
}

/** `graph` (or an empty one) at playback `rate`, 0.5 to 2: 1.25 is faster and higher. */
export function withRate(graph: GraphSpec | null, rate: number): GraphSpec {
  const { rate: _, ...base } = graph ?? { nodes: [], edges: [] };
  return rate === 1 ? base : { ...base, rate };
}

const STEP = /^([A-Za-z0-9_]+)\s*(?:\((.*)\))?$/;

/**
 * Parses the engine's chain notation, e.g. `lowpass(800) => reverb(10, 2)`: node types with
 * optional parameters in the order `catalog` lists them. Throws `bad_request` errors that name
 * the offending step. An empty text is an empty chain.
 */
export function parseChain(text: string, catalog: readonly NodeDescriptor[]): GraphSpec {
  if (text.trim() === "") {
    return chain();
  }
  const effects = text.split("=>").map((raw): Effect => {
    const step = raw.trim();
    const match = STEP.exec(step);
    if (!match) {
      throw badRequest(`malformed step \`${step}\`, expected \`type\` or \`type(1, 2, ...)\``);
    }
    const [, type = "", args = ""] = match;
    const descriptor = catalog.find((node) => node.type === type);
    if (!descriptor) {
      const known = catalog.map((node) => node.type).join(", ");
      throw badRequest(`unknown effect \`${type}\`; available: ${known}`);
    }
    const values = args.trim() === "" ? [] : args.split(",").map((value) => value.trim());
    if (values.length > descriptor.params.length) {
      throw badRequest(`\`${type}\` takes at most ${descriptor.params.length} parameters, got ${values.length}`);
    }
    const params: Record<string, number> = {};
    values.forEach((text, index) => {
      const spec = descriptor.params[index]!;
      const value = Number(text);
      if (text === "" || !Number.isFinite(value)) {
        throw badRequest(`\`${text}\` is not a number (parameter \`${spec.name}\` of \`${type}\`)`);
      }
      if (value < spec.min || value > spec.max) {
        throw badRequest(`\`${spec.name}\` of \`${type}\` must be within ${spec.min} to ${spec.max}, got ${value}`);
      }
      params[spec.name] = value;
    });
    return { type, params };
  });
  return chain(...effects);
}

/** Nodes for `effects`, with ids that are new to `graph`. */
function nodesFor(graph: GraphSpec, effects: readonly Effect[]): NodeSpec[] {
  const taken = new Set([INPUT, OUTPUT, ...graph.nodes.map((node) => node.id)]);
  return effects.map((effect, index) => {
    let id = effect.id;
    if (id === undefined) {
      // numbered by position, like the engine's chains: `lowpass0`, `reverb1`, ...
      let number = graph.nodes.length + index;
      while (taken.has(`${effect.type}${number}`)) {
        number++;
      }
      id = `${effect.type}${number}`;
    } else if (taken.has(id)) {
      throw badRequest(`the graph already has a node \`${id}\``);
    }
    taken.add(id);
    return { id, type: effect.type, params: effect.params ?? {} };
  });
}

/** Edges running `nodes` one after another. */
function links(nodes: readonly NodeSpec[]): Edge[] {
  return nodes.slice(1).map((node, index) => ({ from: nodes[index]!.id, to: node.id }));
}

/** `node` or `node:port`, with the node `from` replaced by `to` and the port kept. */
function renamePort(endpoint: string, from: string, to: string): string {
  const [node, port] = endpoint.split(":");
  if (node !== from) {
    return endpoint;
  }
  return port === undefined ? to : `${to}:${port}`;
}

function badRequest(message: string): EntroximerError {
  return new EntroximerError("bad_request", message);
}
