// The brain map's live mode (`assets/brain-live.js`): the feed path and its
// cursor, the poll rate and backoff, the poller (pauses while hidden, never
// two requests at once, stops when off), the "L" key and the remembered
// choice, the lit nodes, and the feed that focuses a node when picked.

import test from "node:test";
import assert from "node:assert/strict";

import {
  FEED_MAX,
  LIVE_KEY,
  MAX_BACKOFF_MS,
  POLL_MS,
  activityPath,
  bindLive,
  createPoller,
  feedEntry,
  isLiveKey,
  liveEvents,
  nextDelay,
  paintFeed,
  pulseTargets,
  readLivePref,
  relativeTime,
  writeLivePref,
} from "../assets/brain-live.js";

/** A fake clock: `setTimer` queues callbacks; `next()` fires the next one. */
function clock() {
  const queue = [];
  let id = 0;
  return {
    queue,
    setTimer(fn, ms) {
      id += 1;
      queue.push({ id, fn, ms });
      return id;
    },
    clearTimer(handle) {
      const at = queue.findIndex((job) => job.id === handle);
      if (at >= 0) queue.splice(at, 1);
    },
    async next() {
      const job = queue.shift();
      if (job) await job.fn();
      return job;
    },
    get pending() {
      return queue.length;
    },
  };
}

const flush = () => new Promise((resolve) => setImmediate(resolve));

test("the feed path carries the cursor; the first poll has none", () => {
  assert.equal(activityPath(""), "/v1/graph/activity");
  assert.equal(activityPath(null), "/v1/graph/activity");
  assert.equal(activityPath("12"), "/v1/graph/activity?since=12");
});

test("the poll rate holds on success and doubles on failure, to a minute", () => {
  assert.equal(nextDelay(POLL_MS, false), POLL_MS);
  const backoff = [];
  let delay = POLL_MS;
  for (let n = 0; n < 6; n += 1) {
    delay = nextDelay(delay, true);
    backoff.push(delay);
  }
  assert.deepEqual(backoff, [8000, 16000, 32000, MAX_BACKOFF_MS, MAX_BACKOFF_MS, MAX_BACKOFF_MS]);
  assert.equal(nextDelay(MAX_BACKOFF_MS, false), POLL_MS, "one good answer resets it");
});

test("the poller polls with the cursor, lit events go out, and off means no polling", async () => {
  const timers = clock();
  const seen = [];
  const batches = [];
  const answers = [
    { cursor: "5", events: [{ node: "page:a:x", kind: "read", label: "read by Claude" }] },
    { cursor: "9", events: [] },
  ];
  const poller = createPoller({
    fetchActivity: async (cursor) => {
      seen.push(cursor);
      return answers.shift();
    },
    onEvents: (events) => batches.push(events),
    setTimer: timers.setTimer,
    clearTimer: timers.clearTimer,
  });
  assert.equal(timers.pending, 0, "nothing runs before it is started");
  poller.start();
  assert.equal(poller.running, true);
  await timers.next();
  await flush();
  assert.deepEqual(seen, [""]);
  assert.equal(batches.length, 1);
  assert.equal(poller.cursor, "5");
  assert.equal(timers.queue[0].ms, POLL_MS, "the next poll is due at the normal rate");
  await timers.next();
  await flush();
  assert.deepEqual(seen, ["", "5"]);
  assert.equal(batches.length, 1, "an empty answer sends nothing");

  poller.stop();
  assert.equal(timers.pending, 0, "off clears the timer");
  assert.equal(poller.running, false);
});

test("a hidden tab pauses the poll, and showing it again resumes", async () => {
  const timers = clock();
  const doc = { hidden: false };
  let calls = 0;
  const poller = createPoller({
    fetchActivity: async () => {
      calls += 1;
      return { cursor: "1", events: [] };
    },
    onEvents: () => {},
    doc,
    setTimer: timers.setTimer,
    clearTimer: timers.clearTimer,
  });
  poller.start();
  doc.hidden = true;
  poller.visibility();
  assert.equal(timers.pending, 0, "no poll is queued while hidden");
  doc.hidden = false;
  poller.visibility();
  assert.equal(timers.pending, 1, "showing the tab queues a poll");
  await timers.next();
  await flush();
  assert.equal(calls, 1);
  poller.stop();
});

test("a failed poll backs off and reports the error; nothing overlaps", async () => {
  const timers = clock();
  let release;
  let calls = 0;
  const errors = [];
  const poller = createPoller({
    fetchActivity: () => {
      calls += 1;
      if (calls === 1) return Promise.reject(new Error("offline"));
      return new Promise((resolve) => {
        release = () => resolve({ cursor: "2", events: [] });
      });
    },
    onEvents: () => {},
    onError: (error) => errors.push(error.message),
    setTimer: timers.setTimer,
    clearTimer: timers.clearTimer,
  });
  poller.start();
  await timers.next();
  await flush();
  assert.deepEqual(errors, ["offline"]);
  assert.equal(timers.queue[0].ms, 8000, "the retry waits longer");
  // The retry starts and hangs on its answer; a visibility change meanwhile
  // must not queue a second request.
  const retry = timers.next();
  await flush();
  poller.visibility();
  assert.equal(timers.pending, 0, "a request is in flight: nothing else is queued");
  release();
  await retry;
  await flush();
  await flush();
  assert.equal(calls, 2);
  poller.stop();
});

test("the L key toggles, but not while typing, and not with a modifier", () => {
  const field = { tagName: "INPUT" };
  const canvas = { tagName: "CANVAS" };
  const isTyping = (target) => Boolean(target && target.tagName === "INPUT");
  assert.equal(isLiveKey({ key: "l", target: canvas }, isTyping), true);
  assert.equal(isLiveKey({ key: "L", target: canvas }, isTyping), true);
  assert.equal(isLiveKey({ key: "l", target: field }, isTyping), false);
  assert.equal(isLiveKey({ key: "l", ctrlKey: true, target: canvas }, isTyping), false);
  assert.equal(isLiveKey({ key: "k", target: canvas }, isTyping), false);
});

test("the toggle starts and stops polling, follows aria-pressed, and remembers the choice", () => {
  const listeners = {};
  const doc = {
    hidden: false,
    addEventListener(type, fn) {
      (listeners[type] ||= []).push(fn);
    },
  };
  const button = {
    attrs: {},
    listeners: {},
    setAttribute(name, value) {
      this.attrs[name] = value;
    },
    addEventListener(type, fn) {
      this.listeners[type] = fn;
    },
  };
  const store = new Map();
  const storage = {
    getItem: (key) => (store.has(key) ? store.get(key) : null),
    setItem: (key, value) => store.set(key, value),
  };
  const started = [];
  const poller = {
    running: false,
    start() {
      this.running = true;
      started.push("start");
    },
    stop() {
      this.running = false;
      started.push("stop");
    },
    visibility() {},
  };
  const changes = [];
  const live = bindLive({
    button,
    poller,
    doc,
    storage,
    onChange: (on) => changes.push(on),
    isTyping: () => false,
  });
  button.listeners.click();
  assert.equal(poller.running, true);
  assert.equal(button.attrs["aria-pressed"], "true");
  assert.equal(store.get(LIVE_KEY), "1", "the choice is remembered");
  listeners.keydown[0]({ key: "L", target: {}, preventDefault() {} });
  assert.equal(poller.running, false, "L turns it off again");
  assert.equal(button.attrs["aria-pressed"], "false");
  live.set(true, { remember: false });
  assert.equal(store.get(LIVE_KEY), "0", "a restore without remembering leaves the stored choice alone");
  assert.deepEqual(changes, [true, false, true]);
  assert.deepEqual(started, ["start", "stop", "start"]);
});

test("the remembered choice survives a storage that throws", () => {
  const broken = {
    getItem() {
      throw new Error("blocked");
    },
    setItem() {
      throw new Error("blocked");
    },
  };
  assert.equal(readLivePref(broken), false);
  assert.doesNotThrow(() => writeLivePref(broken, true));
  assert.equal(readLivePref(null), false);
  const ok = new Map([[LIVE_KEY, "1"]]);
  assert.equal(readLivePref({ getItem: (k) => ok.get(k) ?? null }), true);
});

test("events light the node they name, and an unknown node is reported, not drawn", () => {
  const nodes = new Map([
    ["page:a:x", 3],
    ["page:a:y", 7],
  ]);
  const events = liveEvents({
    events: [
      { node: "page:a:y", kind: "write", label: "written by Claude", at: "2026-10-17T10:00:00Z", actor: "Claude" },
      { node: "page:a:gone", kind: "read", label: "read by Claude", at: "2026-10-17T10:00:01Z", actor: "Claude" },
      { node: "page:a:x", kind: "search", label: "found by a search from Claude", at: "2026-10-17T10:00:02Z", actor: "Claude" },
      { label: "no node" },
    ],
  });
  assert.equal(events.length, 3, "a malformed event is dropped");
  const { indices, unknown } = pulseTargets(events, (id) => (nodes.has(id) ? nodes.get(id) : -1));
  assert.deepEqual(indices.map((hit) => hit.index), [7, 3]);
  assert.deepEqual(unknown.map((event) => event.node), ["page:a:gone"]);
});

test("the feed reads in plain English, relative to now", () => {
  const now = Date.parse("2026-10-17T10:10:00Z");
  assert.equal(relativeTime("2026-10-17T10:09:50Z", now), "just now");
  assert.equal(relativeTime("2026-10-17T10:07:00Z", now), "3 min ago");
  assert.equal(relativeTime("2026-10-17T07:10:00Z", now), "3 h ago");
  assert.equal(relativeTime("nonsense", now), "");
  const entry = feedEntry({ node: "page:a:x", label: "read by Claude", at: "2026-10-17T10:07:00Z" }, "Alpha", now);
  assert.deepEqual(entry, { node: "page:a:x", text: "Alpha — read by Claude", when: "3 min ago" });
  assert.equal(feedEntry({ node: "n", label: "l", at: "" }, "", now).text, "a page — l");
});

/** A fake element with the bits paintFeed touches. */
function element(tag) {
  return {
    tag,
    children: [],
    listeners: {},
    attrs: {},
    textContent: "",
    className: "",
    type: "",
    setAttribute(name, value) {
      this.attrs[name] = value;
    },
    addEventListener(type, fn) {
      this.listeners[type] = fn;
    },
    appendChild(child) {
      this.children.push(child);
    },
    replaceChildren() {
      this.children = [];
    },
  };
}

test("the feed paints newest first, each line focuses its node when picked, and is capped", () => {
  const doc = { createElement: element };
  const list = element("ol");
  const picked = [];
  const entries = Array.from({ length: FEED_MAX + 5 }, (_, n) => ({
    node: `page:a:p${n}`,
    text: `page ${n}`,
    when: "just now",
  }));
  paintFeed(list, entries, doc, (id) => picked.push(id));
  assert.equal(list.children.length, FEED_MAX);
  const first = list.children[0].children[0];
  assert.equal(first.textContent, "page 0");
  assert.equal(first.attrs["data-node"], "page:a:p0");
  first.listeners.click();
  assert.deepEqual(picked, ["page:a:p0"]);
});
