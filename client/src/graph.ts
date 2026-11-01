import { EntroximerError } from "./errors";
import type { GraphSpec } from "./generated/GraphSpec";
import type { NodeDescriptor } from "./generated/NodeDescriptor";

/** One effect in a chain: a node type (see `NodeInfo.nodes`) and named parameters. */
export interface Effect {
  type: string;
  /** Parameters by name; missing ones take their defaults. */
  params?: Record<string, number>;
}

/** A graph running `effects` one after another. */
export function chain(...effects: Effect[]): GraphSpec {
  return {
    nodes: effects.map((effect, index) => ({
      id: `${effect.type}${index}`,
      type: effect.type,
      params: effect.params ?? {},
    })),
    edges: [],
  };
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
  const effects = text.split("=>").map((raw) => {
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
      throw badRequest(
        `\`${type}\` takes at most ${descriptor.params.length} parameters, got ${values.length}`,
      );
    }
    const params: Record<string, number> = {};
    values.forEach((text, index) => {
      const spec = descriptor.params[index]!;
      const value = Number(text);
      if (text === "" || !Number.isFinite(value)) {
        throw badRequest(`\`${text}\` is not a number (parameter \`${spec.name}\` of \`${type}\`)`);
      }
      if (value < spec.min || value > spec.max) {
        throw badRequest(
          `\`${spec.name}\` of \`${type}\` must be within ${spec.min} to ${spec.max}, got ${value}`,
        );
      }
      params[spec.name] = value;
    });
    return { type, params };
  });
  return chain(...effects);
}

function badRequest(message: string): EntroximerError {
  return new EntroximerError("bad_request", message);
}
