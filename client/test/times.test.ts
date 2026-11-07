import { expect, test } from "bun:test";

import { parseDuration, parseLinkTimes } from "../src/index.js";
import { selector } from "../src/tracks.js";

test("durations parse as seconds, units or clock times", () => {
  const cases: Array<[string, number | null]> = [
    ["90", 90_000],
    ["90.5", 90_500],
    ["90s", 90_000],
    ["1m30s", 90_000],
    ["1m 30s", 90_000],
    ["2M", 120_000],
    ["1h2m3s", 3_723_000],
    ["1h", 3_600_000],
    ["1:30", 90_000],
    ["01:30.25", 90_250],
    ["1:02:03", 3_723_000],
    [" 0:00 ", 0],
    ["", null],
    ["1:75", null],
    ["1:2:3:4", null],
    ["-5", null],
    ["soon", null],
    ["1m30", null],
  ];
  for (const [text, ms] of cases) {
    expect([text, parseDuration(text)]).toEqual([text, ms]);
  }
});

test("YouTube links give their times in the query or the fragment, and lose them", () => {
  expect(parseLinkTimes("https://www.youtube.com/watch?v=I-sH53vXP2A&t=90")).toEqual({
    query: "https://www.youtube.com/watch?v=I-sH53vXP2A",
    startTime: 90_000,
  });
  expect(parseLinkTimes("https://youtu.be/I-sH53vXP2A?t=1m30s")).toEqual({
    query: "https://youtu.be/I-sH53vXP2A",
    startTime: 90_000,
  });
  expect(parseLinkTimes("https://www.youtube.com/embed/I-sH53vXP2A?start=30&end=95&rel=0")).toEqual({
    query: "https://www.youtube.com/embed/I-sH53vXP2A?rel=0",
    startTime: 30_000,
    endTime: 95_000,
  });
  expect(parseLinkTimes("https://music.youtube.com/watch?v=abc&list=RDabc#t=2:05")).toEqual({
    query: "https://music.youtube.com/watch?v=abc&list=RDabc",
    startTime: 125_000,
  });
  expect(parseLinkTimes("https://m.youtube.com/watch?t=0&v=abc")).toEqual({ query: "https://m.youtube.com/watch?v=abc" });
});

test("other sites give theirs as they write them", () => {
  expect(parseLinkTimes("https://www.twitch.tv/videos/123456?t=1h2m3s")).toEqual({
    query: "https://www.twitch.tv/videos/123456",
    startTime: 3_723_000,
  });
  // a channel's `t` is not a time in a video
  expect(parseLinkTimes("https://www.twitch.tv/someone?t=5").startTime).toBeUndefined();
  expect(parseLinkTimes("https://soundcloud.com/artist/track#t=1:23")).toEqual({
    query: "https://soundcloud.com/artist/track",
    startTime: 83_000,
  });
  expect(parseLinkTimes("https://vimeo.com/76979871#t=1m5s")).toEqual({
    query: "https://vimeo.com/76979871",
    startTime: 65_000,
  });
  expect(parseLinkTimes("https://www.dailymotion.com/video/x8abc?start=42")).toEqual({
    query: "https://www.dailymotion.com/video/x8abc",
    startTime: 42_000,
  });
  expect(parseLinkTimes("https://www.bilibili.com/video/BV1xx411c7mD?p=2&t=75.5")).toEqual({
    query: "https://www.bilibili.com/video/BV1xx411c7mD?p=2",
    startTime: 75_500,
  });
});

test("any link's media fragment gives a start, an end or both", () => {
  expect(parseLinkTimes("https://cdn.example.com/a.mp3#t=10,20")).toEqual({
    query: "https://cdn.example.com/a.mp3",
    startTime: 10_000,
    endTime: 20_000,
  });
  expect(parseLinkTimes("https://cdn.example.com/a.mp3#t=npt:,1:00")).toEqual({
    query: "https://cdn.example.com/a.mp3",
    endTime: 60_000,
  });
  // the rest of the link stays exactly as written
  expect(parseLinkTimes("https://example.com/a%20b.ogg?sig=x%2By#page&t=5")).toEqual({
    query: "https://example.com/a%20b.ogg?sig=x%2By#page",
    startTime: 5_000,
  });
});

test("links without times, and other queries, stay as they are", () => {
  for (const query of [
    "ytsearch:fallen kingdom",
    "https://open.spotify.com/track/abc",
    // a cache buster, not a time
    "https://example.com/a.mp3?t=1699999999",
    "https://www.youtube.com/watch?v=abc&t=later",
    "https://example.com/a.mp3#t=a,b",
    "not a link",
  ]) {
    expect(parseLinkTimes(query)).toEqual({ query });
  }
});

test("given times replace a link's or a track's own", () => {
  const link = "https://youtu.be/abc?t=90";
  expect(selector(link)).toEqual({ query: "https://youtu.be/abc", startTime: 90_000 });
  expect(selector(link, { startTime: 5_000.4, endTime: 60_000 })).toEqual({
    query: "https://youtu.be/abc",
    startTime: 5_000,
    endTime: 60_000,
  });
  expect(selector(link, { startTime: null })).toEqual({ query: "https://youtu.be/abc" });
  expect(selector(link, { startTime: 0 })).toEqual({ query: "https://youtu.be/abc" });

  const track = { encoded: "abc", info: {} as never, startTime: 1_000, endTime: 2_000 };
  expect(selector(track)).toEqual({ encoded: "abc", startTime: 1_000, endTime: 2_000 });
  expect(selector(track, { endTime: null })).toEqual({ encoded: "abc", startTime: 1_000 });
});
