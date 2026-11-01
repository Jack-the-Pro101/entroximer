import { describe, expect, test } from "bun:test";

import { EntroximerError, chain, parseChain } from "../src";
import type { NodeDescriptor } from "../src";

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
