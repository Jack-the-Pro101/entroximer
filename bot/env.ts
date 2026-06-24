const REQUIRED_VARS = ["BOT_TOKEN"] as const;

for (const variable of REQUIRED_VARS) {
  if (process.env[variable] == null) {
    throw new Error(`Missing required env var ${variable}. Cannot continue, exiting...`);
  }
}

export const env = Object.freeze({
  BOT_TOKEN: process.env.BOT_TOKEN,
});
