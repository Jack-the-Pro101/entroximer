// Registers the bot's slash commands in one guild, where they appear right away:
//   bun run register <guild id>
import { REST, Routes } from "discord.js";
import type { APIApplication } from "discord.js";

import { commands } from "../src/commands";
import { env } from "../src/env";

const guildId = process.argv[2];
if (!guildId) {
  console.error("usage: bun run register <guild id>");
  process.exit(2);
}

const rest = new REST().setToken(env.BOT_TOKEN);
const application = (await rest.get(Routes.currentApplication())) as APIApplication;
await rest.put(Routes.applicationGuildCommands(application.id, guildId), {
  body: commands.map((command) => command.data.toJSON()),
});
console.log(`registered ${commands.length} commands in guild ${guildId}`);
