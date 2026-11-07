// /graph: how the audio flows, from each layer's source through its effects and volume into the mix,
// as the engine lays it out (`player.view`), in a message only its user sees. Effects can be picked
// to inspect them; a pick names its node by layer, id and graph tag, so it never lands on another
// node that took the same id in a graph set since.
import { ActionRowBuilder, ButtonBuilder, ButtonStyle, ContainerBuilder, MessageFlags, StringSelectMenuBuilder } from "discord.js";
import type { MessageActionRowComponentBuilder, MessageComponentInteraction } from "discord.js";
import type { Player, PlayerView } from "entroximer-client";

import { HINT, LEGEND, RULES, chips, diagram, list, nameOf, nodes } from "../diagram";
import { COLOR } from "../embeds";
import type { Engine } from "../engine";
import { escape, truncate } from "../format";
import { command, failureText } from "./command";
import type { Command } from "./command";
import { existingPlayer } from "./players";

/** How long the view answers its menu and buttons after their last use. */
const IDLE_MS = 10 * 60_000;
/** Room for the drawing, within the 4000 characters of text that a message's components hold. */
const BODY_LIMIT = 3_500;
const MAIN = "main";

/** An effect picked from a view: its layer, its id, and the tag of the graph it was in then. */
interface NodeRef {
  layer: string;
  id: string;
  graphTag: string;
}

type Row = ActionRowBuilder<MessageActionRowComponentBuilder>;

export const graphCommands: Command[] = [
  command(
    (b) =>
      b
        .setName("graph")
        .setDescription("Show how the audio flows: each layer through its effects into the mix")
        .addBooleanOption((o) =>
          o.setName("color").setDescription("Color the diagram; only Discord on a computer shows the colors"),
        ),
    async (interaction, engine) => {
      const flow = new FlowView(existingPlayer(interaction, engine), engine, interaction.options.getBoolean("color") ?? false);
      const response = await interaction.reply({
        components: [await flow.render()],
        flags: MessageFlags.Ephemeral | MessageFlags.IsComponentsV2,
        withResponse: true,
      });
      const message = response.resource?.message;
      if (!message) {
        return;
      }
      const collector = message.createMessageComponentCollector({ idle: IDLE_MS });
      collector.on("collect", (component) => {
        flow.handle(component).catch((error: unknown) => console.error("cannot answer /graph:", error));
      });
      collector.on("end", () => {
        // the menu and buttons stop working: take them away
        interaction.editReply({ components: [flow.closed()] }).catch(() => {});
      });
    },
  ),
];

/** One user's look at a player's signal flow, as they unfold, inspect and refresh it. */
class FlowView {
  readonly #player: Player;
  readonly #engine: Engine;
  readonly #color: boolean;
  /** Layers whose graphs the user unfolded; the others fold into a row when the drawing would not fit. */
  readonly #unfolded = new Set<string>();
  #asText = false;
  /** Whether the rules for reading the diagram are shown. */
  #reading = false;
  #inspecting: NodeRef | null = null;
  /** Said once, above the next drawing. */
  #notice: string | null = null;
  /** The texts last shown, to show without the menu and buttons once they stop working. */
  #shown: string[] = [];

  constructor(player: Player, engine: Engine, color: boolean) {
    this.#player = player;
    this.#engine = engine;
    this.#color = color;
  }

  async handle(component: MessageComponentInteraction): Promise<void> {
    await component.deferUpdate();
    const picked = component.isStringSelectMenu() ? (component.values[0] ?? "") : component.customId;
    const [action = "", ...args] = picked.split(":");
    switch (action) {
      case "text":
        this.#asText = !this.#asText;
        break;
      case "help":
        this.#reading = true;
        break;
      case "back":
        this.#reading = false;
        this.#inspecting = null;
        break;
      case "unfold":
        this.#unfolded.add(args[0] ?? "");
        break;
      case "node": {
        // layer names and node ids have no `:` in them
        const [layer = "", id = "", graphTag = ""] = args;
        this.#inspecting = { layer, id, graphTag };
        break;
      }
      // `refresh` draws it again as it is now
    }
    let shown: ContainerBuilder;
    try {
      shown = await this.render();
    } catch (error) {
      shown = this.#container([`❌ ${failureText(error, "/graph")}`], [buttons(refreshButton())]);
    }
    await component.editReply({ components: [shown] });
  }

  async render(): Promise<ContainerBuilder> {
    if (this.#reading) {
      return this.#container([RULES], [buttons(backButton())]);
    }
    if (this.#inspecting !== null) {
      const inspected = await this.#inspect(this.#inspecting);
      if (inspected !== null) {
        return inspected;
      }
    }
    const { view, body } = await this.#draw();
    const notice = this.#notice;
    this.#notice = null;
    const texts = [["### Signal flow", ...(notice === null ? [] : [notice])].join("\n"), body];
    const drawn = !this.#asText && view.lanes > 1;
    if (drawn) {
      texts.push(`-# ${LEGEND}\n-# ${HINT}`);
    }
    const rows: Row[] = [];
    const options = pickable(view);
    if (options.length > 0) {
      const menu = new StringSelectMenuBuilder()
        .setCustomId("pick")
        .setPlaceholder("Inspect an effect")
        .addOptions(options);
      rows.push(new ActionRowBuilder<MessageActionRowComponentBuilder>().addComponents(menu));
    }
    const toggle = new ButtonBuilder()
      .setCustomId("text")
      .setStyle(ButtonStyle.Secondary)
      .setLabel(this.#asText ? "As a diagram" : "As text");
    const help = new ButtonBuilder().setCustomId("help").setStyle(ButtonStyle.Secondary).setLabel("How to read");
    rows.push(buttons(refreshButton(), toggle, ...(drawn ? [help] : [])));
    return this.#container(texts, rows);
  }

  /** What is shown once the menu and buttons stopped working. */
  closed(): ContainerBuilder {
    return this.#container([...this.#shown, "-# Use `/graph` again to look at it as it is now."], []);
  }

  /**
   * The view and how it is drawn: as it is, or if that is too long for a message, with the graphs
   * of layers the user did not unfold folded into a row each, the music's last.
   */
  async #draw(): Promise<{ view: PlayerView; body: string }> {
    const layers = [MAIN, ...this.#player.layers.keys()];
    const folding = [
      [],
      layers.filter((layer) => layer !== MAIN && !this.#unfolded.has(layer)),
      layers.filter((layer) => !this.#unfolded.has(layer)),
      layers,
    ];
    const tried = new Set<string>();
    let view: PlayerView | null = null;
    for (const collapse of folding) {
      const key = collapse.join();
      if (tried.has(key)) {
        continue;
      }
      tried.add(key);
      view = await this.#player.view({ collapse });
      const body = this.#body(view);
      if (body.length <= BODY_LIMIT) {
        return { view, body };
      }
    }
    // folded as far as it goes and still too long: a list survives being cut
    return { view: view!, body: truncate(list(view!), BODY_LIMIT) };
  }

  #body(view: PlayerView): string {
    if (this.#asText) {
      return list(view);
    }
    return view.lanes > 1 ? diagram(view, this.#color) : chips(view);
  }

  /**
   * An effect's details, if it is still the node that was picked: in the same layer's graph, with
   * the same id, in a graph with the same tag. `null` otherwise, with a notice for the drawing.
   */
  async #inspect(ref: NodeRef): Promise<ContainerBuilder | null> {
    const others = [MAIN, ...this.#player.layers.keys()].filter((layer) => layer !== ref.layer);
    const view = await this.#player.view({ collapse: others });
    const drawn = nodes(view);
    const node = drawn.find((n) => n.kind === "effect" && n.layer === ref.layer && n.id === ref.id);
    const graph = ref.layer === MAIN ? this.#player.graph : (this.#player.layers.get(ref.layer)?.graph ?? null);
    const spec = graph?.nodes.find((n) => n.id === ref.id);
    if (node === undefined || node.graphTag !== ref.graphTag || spec === undefined || spec.type !== node.label) {
      this.#inspecting = null;
      this.#notice = `-# The effects of **${escape(ref.layer)}** changed since \`${ref.id}\` was picked; this is how they are now.`;
      return null;
    }

    const row = view.rows.findIndex((r) => r.node === node);
    const fedBy = node.inputs.map((input) => nameOf(view.rows[input]?.node ?? undefined));
    const feeds = view.rows.flatMap((r) => (r.node?.inputs.includes(row) ? [nameOf(r.node)] : []));
    const descriptor = (await this.#engine.manager.info()).nodes.find((d) => d.type === spec.type);
    const params = (descriptor?.params ?? []).map((param) => {
      const given = spec.params[param.name];
      const set = given === undefined ? "default" : `default ${number(param.default)}`;
      return `- **${param.name}** ${number(given ?? param.default)} (${set}; ${number(param.min)} to ${number(param.max)})`;
    });
    const texts = [
      [
        `### \`${spec.id}\` · ${escape(spec.type)}`,
        `-# An effect of **${escape(ref.layer)}**, fed by ${fedBy.join(", ")}, feeding ${feeds.join(", ")}`,
      ].join("\n"),
      [descriptor?.description ?? "", ...params].filter(Boolean).join("\n"),
    ];
    return this.#container(texts, [buttons(backButton(), refreshButton())]);
  }

  #container(texts: string[], rows: Row[]): ContainerBuilder {
    this.#shown = texts;
    const container = new ContainerBuilder().setAccentColor(COLOR);
    container.addTextDisplayComponents(texts.map((text) => (display) => display.setContent(text)));
    if (rows.length > 0) {
      container.addActionRowComponents(rows);
    }
    return container;
  }
}

/** The menu's options: each effect to inspect, and each folded graph to unfold; 25 at most. */
function pickable(view: PlayerView): Array<{ label: string; description: string; value: string }> {
  return nodes(view)
    .flatMap((node) => {
      if (node.kind === "effect" && node.layer !== null && node.id !== null && node.graphTag !== null) {
        return [
          {
            label: truncate(`${node.id} · ${node.label}`, 100),
            description: truncate([node.layer, node.detail].filter(Boolean).join(" · "), 100),
            value: `node:${node.layer}:${node.id}:${node.graphTag}`,
          },
        ];
      }
      if (node.kind === "graph" && node.layer !== null) {
        return [
          {
            label: truncate(`${node.layer}: ${node.label}`, 100),
            description: "Show them one by one",
            value: `unfold:${node.layer}`,
          },
        ];
      }
      return [];
    })
    .slice(0, 25);
}

function backButton(): ButtonBuilder {
  return new ButtonBuilder().setCustomId("back").setStyle(ButtonStyle.Secondary).setLabel("Back");
}

function refreshButton(): ButtonBuilder {
  return new ButtonBuilder().setCustomId("refresh").setStyle(ButtonStyle.Secondary).setLabel("Refresh");
}

function buttons(...components: ButtonBuilder[]): Row {
  return new ActionRowBuilder<MessageActionRowComponentBuilder>().addComponents(components);
}

/** A parameter's value as people read it: to two decimals at most. */
function number(value: number): string {
  return String(Math.round(value * 100) / 100);
}
