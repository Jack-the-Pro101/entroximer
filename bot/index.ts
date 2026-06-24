import { Client, Events, GatewayIntentBits } from "discord.js";
import { env } from "./env";

const client = new Client({
  intents: [GatewayIntentBits.Guilds, GatewayIntentBits.GuildVoiceStates],
});

client.once(Events.ClientReady, (readyClient) => {
  console.log(`Bot logged in as ${readyClient.user.username}`);
});

await client.login(env.BOT_TOKEN);
