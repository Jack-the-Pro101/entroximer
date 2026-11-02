import { expect, test } from "bun:test";

import packageJson from "../package.json" with { type: "json" };
import { CLIENT_NAME } from "../src/connection.js";

test("the client names itself with the package's version", () => {
  expect(CLIENT_NAME).toBe(`${packageJson.name}/${packageJson.version}`);
});
