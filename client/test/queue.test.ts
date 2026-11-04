import { expect, test } from "bun:test";

import type { Requester } from "../src/connection.js";
import type { QueueItem } from "../src/index.js";
import { Queue } from "../src/queue.js";

function item(id: string, loading: boolean): QueueItem {
  return {
    id,
    loading,
    track: {
      encoded: id,
      userData: null,
      info: {
        identifier: id,
        title: id,
        author: null,
        authorUrl: null,
        album: null,
        length: null,
        isStream: false,
        isSeekable: true,
        uri: id,
        artworkUrl: null,
        isrc: null,
        sourceName: loading ? "query" : "http",
      },
    },
  };
}

test("an item added while the queue syncs is waited for once the sync is done", async () => {
  // the engine's answer to the sync waits until the test lets it through
  let answer = (): void => undefined;
  const answered = new Promise<void>((resolve) => {
    answer = resolve;
  });
  const request = (async () => {
    await answered;
    return { queue: { version: 0, length: 0, repeat: "off", crossfade: 0 }, offset: 0, items: [] };
  }) as unknown as Requester;
  const queue = new Queue("1", request, (error) => {
    throw error;
  });

  const syncing = queue.sync();
  queue._handleChange(1, { type: "added", index: 0, items: [item("a", true)] });
  const loaded = queue.whenLoaded("a");
  answer();
  await syncing;
  expect(queue.items.map((it) => it.id)).toEqual(["a"]);
  queue._handleChange(2, { type: "loaded", id: "a", items: [item("a", false)], playlist: null });
  expect((await loaded).items.map((it) => it.loading)).toEqual([false]);
});
