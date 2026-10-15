// The service worker (`sw.js`, as `tools/stamp-assets.mjs` builds it) run in
// a VM context with an in-memory Cache Storage and a scripted `fetch`, and the
// page side of an update (`assets/update.js`) on stub objects.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

import { build } from "../../tools/stamp-assets.mjs";
import {
  askBuild,
  isCurrentBuild,
  pageAssets,
  showUpdateNotice,
  startUpdateWatch,
  watchForUpdates,
  UPDATE_MESSAGE,
} from "../assets/update.js";

const APP = fileURLToPath(new URL("..", import.meta.url));
const OUT = mkdtempSync(join(tmpdir(), "lb-sw-test-"));
const built = build(APP, OUT);
const BUILT_SW = readFileSync(join(OUT, "sw.js"), "utf8");
rmSync(OUT, { recursive: true, force: true });
const ORIGIN = "https://app.example";

/** A Cache Storage keyed by URL string. */
function cacheStorage(initial = {}) {
  const stores = new Map(Object.entries(initial).map(([k, v]) => [k, new Map(v)]));
  const keyOf = (r) => (typeof r === "string" ? new URL(r, ORIGIN).href : r.url);
  const api = {
    stores,
    open: async (name) => {
      if (!stores.has(name)) stores.set(name, new Map());
      const store = stores.get(name);
      return {
        put: async (req, res) => void store.set(keyOf(req), res),
        match: async (req) => store.get(keyOf(req)),
      };
    },
    keys: async () => [...stores.keys()],
    delete: async (name) => stores.delete(name),
    match: async (req) => {
      for (const store of stores.values()) {
        const hit = store.get(keyOf(req));
        if (hit) return hit;
      }
      return undefined;
    },
  };
  return api;
}

/** Loads a worker script; returns its listeners and the mocks it saw. */
function loadWorker(source, { caches = cacheStorage(), fetch } = {}) {
  const listeners = {};
  const calls = { skipWaiting: 0, claim: 0, fetched: [] };
  const self = {
    location: new URL(`${ORIGIN}/sw.js`),
    addEventListener: (type, fn) => {
      listeners[type] = fn;
    },
    skipWaiting: async () => void (calls.skipWaiting += 1),
    clients: { claim: async () => void (calls.claim += 1) },
  };
  const context = vm.createContext({
    self,
    caches,
    URL,
    Response,
    Promise,
    fetch: async (input, init) => {
      const url = typeof input === "string" ? input : input.url;
      calls.fetched.push({ url, init });
      return fetch(url, init);
    },
  });
  vm.runInContext(source, context);
  return { listeners, calls, caches };
}

/** Dispatches an extendable event and waits for everything it waits on. */
async function dispatch(worker, type, extra = {}) {
  const waits = [];
  let responded;
  const event = {
    ...extra,
    waitUntil: (p) => waits.push(p),
    respondWith: (p) => {
      responded = p;
    },
  };
  worker.listeners[type](event);
  const response = responded ? await responded : undefined;
  // waitUntil may be called while the response settles; drain until quiet.
  for (let i = 0; i < waits.length; i += 1) await waits[i];
  return response;
}

const ok = (body, extra = {}) => new Response(body, { status: 200, ...extra });
const request = (path, mode = "cors") => ({ url: `${ORIGIN}${path}`, method: "GET", mode });

const VERSION = /const VERSION = "([^"]+)"/.exec(BUILT_SW)[1];
const CACHE = `lb-app-${VERSION}`;

test("install precaches the build's shell, under clean keys, then activates at once", async () => {
  const worker = loadWorker(BUILT_SW, { fetch: async (url) => ok(`body of ${url}`) });
  await dispatch(worker, "install");
  assert.equal(worker.calls.skipWaiting, 1);
  const store = worker.caches.stores.get(CACHE);
  assert.ok(store, `cache ${CACHE} exists`);
  const appJs = `${ORIGIN}/${built.names.get("assets/app.js")}`;
  assert.ok(store.has(appJs), "hashed app.js precached");
  // Pages are keyed by their canonical address: `/settings.html` → `/settings`.
  assert.ok(store.has(`${ORIGIN}/settings`));
  assert.ok(store.has(`${ORIGIN}/`));
  assert.ok(!store.has(`${ORIGIN}/settings.html`));
  // The precache skips the HTTP cache, so it never stores a pre-deploy copy.
  assert.ok(worker.calls.fetched.every((c) => c.init && c.init.cache === "no-cache"));
});

test("a failed precache still activates the worker", async () => {
  const worker = loadWorker(BUILT_SW, {
    fetch: async () => {
      throw new TypeError("offline");
    },
  });
  await dispatch(worker, "install");
  assert.equal(worker.calls.skipWaiting, 1);
});

test("activate drops every older lb-app cache, keeps others, and claims the tabs", async () => {
  const caches = cacheStorage({ "lb-app-v15": [], "lb-app-0123456789ab": [], [CACHE]: [], other: [] });
  const worker = loadWorker(BUILT_SW, { caches, fetch: async () => ok("") });
  await dispatch(worker, "activate");
  assert.deepEqual([...caches.stores.keys()].sort(), [CACHE, "other"].sort());
  assert.equal(worker.calls.claim, 1);
});

test("hashed assets are cache first: a hit never touches the network", async () => {
  const path = `/${built.names.get("assets/github.js")}`;
  const caches = cacheStorage({ [CACHE]: [[`${ORIGIN}${path}`, ok("cached")]] });
  const worker = loadWorker(BUILT_SW, {
    caches,
    fetch: async () => {
      throw new Error("must not fetch");
    },
  });
  const response = await dispatch(worker, "fetch", { request: request(path) });
  assert.equal(await response.text(), "cached");
  assert.equal(worker.calls.fetched.length, 0);
});

test("a hashed miss is fetched once and stored", async () => {
  const path = "/static/assets/new.abcdef0123.js";
  const worker = loadWorker(BUILT_SW, { fetch: async () => ok("fresh") });
  const response = await dispatch(worker, "fetch", { request: request(path) });
  assert.equal(await response.text(), "fresh");
  assert.ok(worker.caches.stores.get(CACHE).has(`${ORIGIN}${path}`));
});

test("pages are network first, and the cached copy answers only when offline", async () => {
  let online = true;
  const worker = loadWorker(BUILT_SW, {
    fetch: async () => {
      if (!online) throw new TypeError("offline");
      return ok("<p>new</p>");
    },
  });
  const nav = request("/settings", "navigate");
  assert.equal(await (await dispatch(worker, "fetch", { request: nav })).text(), "<p>new</p>");
  online = false;
  // `/settings.html?tab=x` is the same page as `/settings`.
  const offline = await dispatch(worker, "fetch", { request: request("/settings.html?tab=x", "navigate") });
  assert.equal(await offline.text(), "<p>new</p>");
});

test("a page is fetched even when an older copy is cached", async () => {
  const caches = cacheStorage({ "lb-app-old": [[`${ORIGIN}/wiki`, ok("old")]] });
  const worker = loadWorker(BUILT_SW, { caches, fetch: async () => ok("new") });
  const response = await dispatch(worker, "fetch", { request: request("/wiki", "navigate") });
  assert.equal(await response.text(), "new");
});

test("the API, other origins and writes are never intercepted", async () => {
  const worker = loadWorker(BUILT_SW, { fetch: async () => ok("x") });
  for (const req of [
    request("/v1/me"),
    { url: "https://elsewhere.example/a.js", method: "GET", mode: "cors" },
    { url: `${ORIGIN}/wiki`, method: "POST", mode: "navigate" },
  ]) {
    let responded = false;
    worker.listeners.fetch({ request: req, respondWith: () => (responded = true), waitUntil: () => {} });
    assert.equal(responded, false, req.url);
  }
});

test("the unbuilt worker runs too: no precache, dev cache, same strategies", async () => {
  const source = readFileSync(new URL("../sw.js", import.meta.url), "utf8");
  assert.match(source, /const VERSION = "dev";/);
  assert.match(source, /const PRECACHE = \[\];/);
  const worker = loadWorker(source, { fetch: async () => ok("x") });
  await dispatch(worker, "install");
  assert.equal(worker.calls.fetched.length, 0);
  assert.equal(worker.calls.skipWaiting, 1);
});

// --- the page side --------------------------------------------------------

function container(controller) {
  const handlers = {};
  return {
    controller,
    updates: 0,
    addEventListener: (type, fn) => {
      handlers[type] = fn;
    },
    getRegistration() {
      return Promise.resolve({ update: () => void (this.updates += 1) });
    },
    fire: (type) => handlers[type](),
  };
}

const tick = () => new Promise((r) => setTimeout(r, 0));

test("a hand-over to a different build offers a reload, once", async () => {
  const sw = container({});
  let shown = 0;
  assert.equal(watchForUpdates(sw, () => (shown += 1), { isCurrent: async () => false }), true);
  sw.fire("controllerchange");
  sw.fire("controllerchange");
  await tick();
  assert.equal(shown, 1);
});

test("a hand-over to the build this tab already runs says nothing", async () => {
  const sw = container(null);
  let shown = 0;
  watchForUpdates(sw, () => (shown += 1), { isCurrent: async () => true });
  sw.fire("controllerchange");
  await tick();
  assert.equal(shown, 0);
});

test("the worker answers which build it is, and the tab compares its own files", async () => {
  const worker = loadWorker(BUILT_SW, { fetch: async () => ok("") });
  const { port1, port2 } = new MessageChannel();
  const answer = new Promise((resolve) => (port1.onmessage = (e) => resolve(e.data)));
  worker.listeners.message({ data: { type: "lb-build" }, ports: [port2] });
  const data = await answer;
  port1.close();
  assert.equal(data.version, VERSION);
  assert.deepEqual(data.precache, built.precache);

  const appJs = `${ORIGIN}/${built.names.get("assets/app.js")}`;
  const css = `${ORIGIN}/${built.names.get("assets/app.css")}`;
  const doc = { querySelectorAll: () => [{ href: css }, { href: `${ORIGIN}/assets/app.css` }] };
  const mine = pageAssets(doc, appJs);
  assert.deepEqual(mine, [appJs, css], "only the hashed files count");
  assert.equal(isCurrentBuild(data, mine, ORIGIN), true);
  const stale = [`${ORIGIN}/static/assets/app.0000000000.js`, css];
  assert.equal(isCurrentBuild(data, stale, ORIGIN), false);
  assert.equal(isCurrentBuild(null, mine, ORIGIN), false, "no answer: assume it changed");
  assert.equal(isCurrentBuild({ precache: [] }, mine, ORIGIN), true, "the unbuilt worker never nags");
  assert.equal(isCurrentBuild(data, [], ORIGIN), true, "nor does the unbuilt page");
});

test("askBuild talks over a MessageChannel and gives up quietly", async () => {
  const controller = {
    postMessage: (msg, [port]) => port.postMessage({ type: msg.type, precache: ["./x"] }),
  };
  assert.deepEqual(await askBuild(controller), { type: "lb-build", precache: ["./x"] });
  assert.equal(await askBuild({ postMessage: () => {} }, 10), null);
  assert.equal(await askBuild(null), null);
});

test("startUpdateWatch shows the notice for a tab on an older build", async () => {
  const sw = container({});
  sw.controller = {
    postMessage: (msg, [port]) => port.postMessage({ precache: ["./static/assets/app.1111111111.js"] }),
  };
  const shown = [];
  const doc = {
    location: { origin: ORIGIN },
    querySelectorAll: () => [],
    body: { appendChild: (n) => shown.push(n) },
    createElement: () => ({ dataset: {}, setAttribute() {}, appendChild() {}, addEventListener() {} }),
    getElementById: () => null,
  };
  startUpdateWatch(sw, doc, `${ORIGIN}/static/assets/app.2222222222.js`, () => {});
  sw.fire("controllerchange");
  await new Promise((r) => setTimeout(r, 20));
  assert.equal(shown.length, 1);
});

test("coming back to the tab asks the browser to check for a new worker", async () => {
  const sw = container({});
  const handlers = {};
  const doc = { visibilityState: "hidden", addEventListener: (t, fn) => (handlers[t] = fn) };
  watchForUpdates(sw, () => {}, { doc });
  handlers.visibilitychange();
  doc.visibilityState = "visible";
  handlers.visibilitychange();
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(sw.updates, 1);
});

test("no service worker support is a quiet no-op", () => {
  assert.equal(watchForUpdates(undefined, () => {}), false);
});

test("the notice is text and a button, built without HTML, shown once", () => {
  const made = [];
  const el = (tag) => {
    const node = {
      tag,
      dataset: {},
      attrs: {},
      children: [],
      listeners: {},
      setAttribute: (k, v) => (node.attrs[k] = v),
      appendChild: (c) => node.children.push(c),
      addEventListener: (t, fn) => (node.listeners[t] = fn),
    };
    made.push(node);
    return node;
  };
  const body = el("body");
  const doc = {
    body,
    createElement: el,
    getElementById: (id) => body.children.find((c) => c.id === id) || null,
  };
  let reloads = 0;
  const box = showUpdateNotice(doc, () => (reloads += 1));
  assert.equal(box.id, "lb-update");
  assert.equal(box.attrs.role, "status");
  assert.equal(box.children[0].textContent, UPDATE_MESSAGE);
  const button = box.children[1];
  assert.equal(button.textContent, "Reload");
  button.listeners.click();
  assert.equal(reloads, 1);
  assert.equal(showUpdateNotice(doc, () => {}), box, "a second call reuses it");
  assert.equal(body.children.length, 1);
});
