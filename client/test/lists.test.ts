import { expect, test } from "bun:test";

import { parseQueries } from "../src/index.js";
import { selector } from "../src/tracks.js";

test("lists of queries keep their order and leave out blanks and comments", () => {
  const text = "https://youtu.be/I-sH53vXP2A\r\n\n  # Spotify next\n https://open.spotify.com/track/abc \nscsearch:fallen kingdom\n";
  expect(parseQueries(text)).toEqual([
    "https://youtu.be/I-sH53vXP2A",
    "https://open.spotify.com/track/abc",
    "scsearch:fallen kingdom",
  ]);
  expect(parseQueries(" \n# nothing\n")).toEqual([]);
});

test("a start index goes with queries only", () => {
  expect(selector("https://example.com/list", { userData: { by: "me" }, startIndex: 4.6 })).toEqual({
    query: "https://example.com/list",
    startIndex: 5,
    userData: { by: "me" },
  });
  expect(selector("ytsearch:x")).toEqual({ query: "ytsearch:x" });
  const track = { encoded: "abc", info: {} as never };
  expect(selector(track, { startIndex: 3 })).toEqual({ encoded: "abc" });
});
