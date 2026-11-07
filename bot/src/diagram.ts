// Drawing a player's signal flow, as the engine lays it out (`player.view`), in Discord: as a
// diagram in a code block, as a row of chips when it is one plain chain, or as a list, which
// reads well aloud.
import type { PlayerView, ViewNode, ViewRow } from "entroximer-client";

import { escape, truncate } from "./format";

/** How wide a diagram's lines get before their details move to lines of their own: about what a phone shows. */
const LINE_WIDTH = 48;
/** The most lines a node's details take below it; the rest is cut. */
const DETAIL_LINES = 3;
/** How far a node's details are indented below it, so that they do not look like steps of their own. */
const DETAIL_INDENT = "  ";

/** What the glyphs in a diagram stand for. */
export const LEGEND = "■ source · ● effect · ◊ effects, folded · ○ rate or volume · □ mix";
/** The gist of reading a diagram, to show under it with the legend. */
export const HINT = "Top to bottom; each column is one path, and paths meet only where they join.";

/** The full rules for reading a diagram. */
export const RULES = Object.freeze([
  "### Diagram Legend",
  "Top to bottom, audio flows downward. Each row with a symbol is one step the audio goes through, and each column of `│` is a separate path the audio takes.",
  "- **Symbols are steps:** `■` is where a layer starts (what it plays), `●` an effect, `○` the playback rate or the volume, `◊` a layer's effects folded into one row, and `□` the final mix, which goes through the limiter into the voice channel.",
  "- **`├─┐` splits:** the audio is copied into a new path to the right; `├─┬─┐` copies it into two more.",
  "- **`├─┘` joins:** paths are added back together; `├─┴─┘` joins three.",
  "- **`┼` crosses:** one path passing over another without touching it.",
  "- **The text after a symbol** names the step: an effect's id, then its type and settings; what a layer plays; a volume. Indented lines without a symbol continue the step above them.",
  "Pick an effect in the menu to see what it does and all its settings, or a folded `◊` row to unfold it.",
].join("\n"));

/** Gutter characters a wire comes down into from the row above. */
const FROM_ABOVE = new Set(["│", "├", "┼", "┴", "┘", "●", "○", "◊", "□"]);
const NODE_GLYPHS = new Set(["■", "●", "○", "◊", "□"]);

/** ANSI colors Discord shows, for each layer in turn: the music green, then blue, pink, yellow and cyan. */
const LAYER_COLORS = [32, 34, 35, 33, 36];
const ESC = "\u001b";

/** The diagram, in a code block; colored with ANSI codes (which only desktop Discord shows) if `color`. */
export function diagram(view: PlayerView, color = false): string {
  const layers = [...new Set(view.rows.flatMap((row) => row.node?.layer ?? []))];
  const paint = (text: string, code: string) => (color && text ? `${ESC}[${code}m${text}${ESC}[0m` : text);
  const layerColor = (node: ViewNode) =>
    node.layer === null ? "37" : String(LAYER_COLORS[layers.indexOf(node.layer) % LAYER_COLORS.length]);

  const lines = view.rows.flatMap((row, index) => {
    if (row.node === null) {
      return [row.gutter.trimEnd()];
    }
    const node = row.node;
    const lines: string[] = [];
    if (node.kind === "source" && node.layer !== null) {
      lines.push(heading(row.gutter, paint(plain(node.layer), `1;${layerColor(node)}`)));
    }
    const { head, rest } = describe(node);
    const gutter = [...row.gutter].map((char) => (NODE_GLYPHS.has(char) ? paint(char, layerColor(node)) : char)).join("");
    const room = Math.max(12, LINE_WIDTH - [...row.gutter].length - 1);
    const first = `${gutter} ${paint(truncate(head, room), `1;${layerColor(node)}`)}`;
    if (rest === null) {
      lines.push(first);
    } else if (head.length + 3 + rest.length <= room) {
      lines.push(`${first}${paint(` · ${rest}`, "30")}`);
    } else {
      // the details go below, indented, with the wires that run on past them
      const below = wiresBelow(view.rows, index);
      const parts = wrap(rest, room - DETAIL_INDENT.length);
      lines.push(first, ...parts.map((part) => `${below} ${DETAIL_INDENT}${paint(part, "30")}`));
    }
    return lines;
  });
  return `\`\`\`${color ? "ansi" : ""}\n${lines.join("\n")}\n\`\`\``;
}

/** A plain chain (one lane) as chips: `**main** Song → \`low\` lowpass, cutoff 220 → volume 80% → mix`. */
export function chips(view: PlayerView): string {
  return nodes(view)
    .map((node) => {
      switch (node.kind) {
        case "source":
          return `**${escape(node.layer ?? "")}** ${escape(node.label)}${node.detail ? ` (${escape(node.detail)})` : ""}`;
        case "effect": {
          const what = [typeOf(node), node.detail].filter((part) => part !== null).join(", ");
          return `\`${node.id}\`${what ? ` ${escape(what)}` : ""}`;
        }
        default:
          return escape(node.detail ? `${node.label}, ${node.detail}` : node.label);
      }
    })
    .join(" → ");
}

/**
 * The flow as a list, for screen readers and small screens: each layer's source as a heading, its
 * nodes below in the order the audio passes them, and where a node is fed by something other than
 * the one before it, what feeds it.
 */
export function list(view: PlayerView): string {
  const byRow = new Map(view.rows.flatMap((row, index) => (row.node ? [[index, row.node] as const] : [])));
  let previous: number | null = null;
  const lines: string[] = [];
  for (const [index, node] of byRow) {
    const fedBy = node.inputs.length === 1 && node.inputs[0] === previous ? [] : node.inputs;
    const from = fedBy.length === 0 ? "" : `, from ${fedBy.map((row) => nameOf(byRow.get(row))).join(", ")}`;
    const detail = node.detail ? ` · ${escape(node.detail)}` : "";
    switch (node.kind) {
      case "source":
        lines.push(`**${escape(node.layer ?? "")}**: ${escape(node.label)}${detail}`);
        break;
      case "mix":
        lines.push(`**${escape(node.label)}**${detail}${from}`);
        break;
      case "effect": {
        const type = typeOf(node);
        lines.push(`- \`${node.id}\`${type === null ? "" : ` ${escape(type)}`}${detail}${from}`);
        break;
      }
      default:
        lines.push(`- ${escape(node.label)}${detail}${from}`);
    }
    previous = index;
  }
  return lines.join("\n");
}

/** The nodes of `view`, in the order they are drawn. */
export function nodes(view: PlayerView): ViewNode[] {
  return view.rows.flatMap((row) => (row.node ? [row.node] : []));
}

/** What a diagram's row says: its node's name (for a source, what it plays: the layer heads its column), and the rest. */
function describe(node: ViewNode): { head: string; rest: string | null } {
  const label = plain(node.label);
  const detail = node.detail === null ? null : plain(node.detail);
  const joined = (...parts: Array<string | null>) => parts.filter((part) => part !== null).join(" · ") || null;
  switch (node.kind) {
    case "effect": {
      const type = typeOf(node);
      return { head: node.id ?? label, rest: joined(type === null ? null : plain(type), detail) };
    }
    default:
      return { head: label, rest: detail };
  }
}

/** An effect's node type, unless its id says it already, as in `limiter` the limiter. */
function typeOf(node: ViewNode): string | null {
  return node.label === node.id ? null : node.label;
}

/** How a node is named where it feeds another: an effect by its id, the volume of a layer as the layer. */
export function nameOf(node: ViewNode | undefined): string {
  if (!node) {
    return "?";
  }
  if (node.id !== null) {
    return `\`${node.id}\``;
  }
  const layer = escape(node.layer ?? "");
  return node.kind === "volume" ? layer : `${layer}'s ${node.kind}`;
}

/** `text` in lines of at most `width`, broken after its ` · ` where it can, in `DETAIL_LINES` at most. */
function wrap(text: string, width: number): string[] {
  const lines: string[] = [];
  for (const part of text.split(" · ")) {
    const last = lines.at(-1);
    if (last !== undefined && last.length + 3 + part.length <= width) {
      lines[lines.length - 1] = `${last} · ${part}`;
    } else {
      lines.push(part);
    }
  }
  const kept = lines.slice(0, DETAIL_LINES).map((line) => truncate(line, width));
  if (lines.length > DETAIL_LINES) {
    kept[DETAIL_LINES - 1] = truncate(`${kept[DETAIL_LINES - 1]} · …`, width);
  }
  return kept;
}

/**
 * A layer's name over its column, from the gutter of its source's row: the name where the source's
 * symbol is, after the wires passing on its left; after every wire if some pass on its right too.
 */
function heading(gutter: string, name: string): string {
  const cells = [...gutter];
  const at = cells.findIndex((char) => NODE_GLYPHS.has(char));
  if (cells.slice(at + 1).some((char) => char !== " ")) {
    return `${cells.map((char) => (NODE_GLYPHS.has(char) ? " " : char)).join("")} ${name}`;
  }
  return `${cells.slice(0, at).join("")}${name}`;
}

/** The wires running on from row `index` down into the next, for a line of text in between. */
function wiresBelow(rows: readonly ViewRow[], index: number): string {
  const next = rows[index + 1];
  const width = [...rows[index]!.gutter].length;
  if (!next) {
    return " ".repeat(width);
  }
  // a node nothing feeds starts its wire on its own row
  const startsWire = next.node !== null && next.node.inputs.length === 0;
  return [...next.gutter]
    .map((char) => (FROM_ABOVE.has(char) && !(startsWire && NODE_GLYPHS.has(char)) ? "│" : " "))
    .join("");
}

/** Text that cannot break out of a code block, or color it. */
function plain(text: string): string {
  // eslint-disable-next-line no-control-regex
  return text.replace(/[\u0000-\u001f\u007f]/g, "").replaceAll("`", "ˋ");
}
