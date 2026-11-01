import { describe, expect, test } from "bun:test";

import { EntroximerError, append, chain, parseChain, prepend, withParams, withRate } from "../src";
import type { GraphSpec, NodeDescriptor } from "../src";

const catalog: NodeDescriptor[] = [
  {
    type: "lowpass",
    description: "",
    params: [
      { name: "cutoff", description: "", default: 1000, min: 20, max: 20000 },
      { name: "q", description: "", default: 0.7, min: 0.1, max: 20 },
    ],
  },
  { type: "reverb", description: "", params: [] },
];

describe("chain", () => {
  test("numbers nodes in order and leaves edges to the engine", () => {
    expect(chain({ type: "lowpass", params: { cutoff: 800 } }, { type: "reverb" })).toEqual({
      nodes: [
        { id: "lowpass0", type: "lowpass", params: { cutoff: 800 } },
        { id: "reverb1", type: "reverb", params: {} },
      ],
      edges: [],
    });
  });
});

describe("parseChain", () => {
  test("maps positional values to parameter names", () => {
    expect(parseChain(" lowpass(800, 2) => reverb ", catalog)).toEqual(
      chain({ type: "lowpass", params: { cutoff: 800, q: 2 } }, { type: "reverb", params: {} }),
    );
    expect(parseChain("lowpass()", catalog).nodes[0]?.params).toEqual({});
    expect(parseChain("  ", catalog)).toEqual(chain());
  });

  test.each([
    ["lowpass(800", "malformed"],
    ["warp", "unknown effect `warp`; available: lowpass, reverb"],
    ["lowpass(1, 2, 3)", "at most 2"],
    ["lowpass(loud)", "not a number"],
    ["lowpass(5)", "within 20 to 20000"],
    ["lowpass(800,)", "not a number"],
  ])("rejects %p", (text, message) => {
    let error: unknown;
    try {
      parseChain(text, catalog);
    } catch (thrown) {
      error = thrown;
    }
    expect(error).toBeInstanceOf(EntroximerError);
    expect((error as EntroximerError).code).toBe("bad_request");
    expect((error as EntroximerError).message).toContain(message);
  });
});

/** The karaoke preset's shape: two branches between input and output. */
const branched: GraphSpec = {
  nodes: [
    { id: "low", type: "lowpass", params: { cutoff: 220 } },
    { id: "high", type: "highpass", params: { cutoff: 220 } },
  ],
  edges: [
    { from: "input", to: "low" },
    { from: "input", to: "high" },
    { from: "low", to: "output" },
    { from: "high:1", to: "output:0" },
  ],
};

describe("building on graphs", () => {
  test("appending to a chain extends it", () => {
    const graph = append(chain({ type: "lowpass" }), { type: "reverb", params: { mix: 0.2 } });
    expect(graph.nodes.map((node) => node.id)).toEqual(["lowpass0", "reverb1"]);
    expect(graph.edges).toEqual([]);
    expect(append(null, { type: "reverb", id: "room" }).nodes[0]?.id).toBe("room");
  });

  test("appending to wired graphs takes over the output, ports included", () => {
    const graph = append(branched, { type: "reverb", id: "room" }, { type: "limiter" });
    expect(graph.edges).toEqual([
      { from: "input", to: "low" },
      { from: "input", to: "high" },
      { from: "low", to: "room" },
      { from: "high:1", to: "room:0" },
      { from: "room", to: "limiter3" },
      { from: "limiter3", to: "output" },
    ]);
  });

  test("prepending to wired graphs takes over the input", () => {
    const graph = prepend(branched, { type: "volume", id: "gain" });
    expect(graph.nodes[0]?.id).toBe("gain");
    expect(graph.edges.slice(0, 3)).toEqual([
      { from: "input", to: "gain" },
      { from: "gain", to: "low" },
      { from: "gain", to: "high" },
    ]);
  });

  test("parameters change in place and rates come and go", () => {
    const tuned = withParams(branched, "low", { q: 2 });
    expect(tuned.nodes[0]?.params).toEqual({ cutoff: 220, q: 2 });
    expect(branched.nodes[0]?.params).toEqual({ cutoff: 220 });
    expect(() => withParams(branched, "nope", {})).toThrow("has no node `nope`");

    const fast = withRate(branched, 1.25);
    expect(fast.rate).toBe(1.25);
    expect("rate" in withRate(fast, 1)).toBe(false);
  });

  test("ids stay unique", () => {
    expect(() => append(branched, { type: "lowpass", id: "low" })).toThrow("already has a node `low`");
    const graph = append(chain({ type: "lowpass" }, { type: "lowpass" }), { type: "lowpass" });
    expect(new Set(graph.nodes.map((node) => node.id)).size).toBe(3);
  });
});
