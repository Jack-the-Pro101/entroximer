// The development bot: slash commands that drive the engine through the client library.
// Run `bun run register <guild id>` once to make the commands appear in a guild.
import { Client, Events, GatewayIntentBits } from "discord.js";

import { run, suggest } from "./commands";
import { connectEngine } from "./engine";
import { env } from "./env";

const client = new Client({
  intents: [GatewayIntentBits.Guilds, GatewayIntentBits.GuildVoiceStates],
});

client.once(Events.ClientReady, async (ready) => {
  console.log(`logged in as ${ready.user.tag}`);
  const engine = await connectEngine(ready).catch((error: unknown) => {
    console.error(error instanceof Error ? error.message : error);
    process.exit(1);
  });

  ready.on(Events.InteractionCreate, (interaction) => {
    if (!interaction.inCachedGuild()) {
      return;
    }
    const answering = interaction.isChatInputCommand()
      ? run(interaction, engine)
      : interaction.isAutocomplete()
        ? suggest(interaction, engine)
        : null;
    answering?.catch((error: unknown) => console.error("cannot answer an interaction:", error));
  });

  for (const signal of ["SIGINT", "SIGTERM"] as const) {
    process.once(signal, () => {
      // the engine pauses the players and destroys them once the resume timeout passes
      engine.manager.close();
      void ready.destroy().then(() => process.exit(0));
    });
  }
});

await client.login(env.BOT_TOKEN);
