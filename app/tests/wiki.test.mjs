// The wiki page, booted for real.
//
// `pages.wiki` is wired by `boot()` when the module can see a document, so
// these tests install the minimal DOM a wiki page has — stub elements keyed by
// id, a `location`, a `history`, a `window` and a recorded `fetch` — *before*
// importing `assets/app.js`. The import then runs the same `boot()` the
// browser runs, and the tests drive the page's real click handlers rather
// than a re-implementation.
//
// That is also how the TDZ regression behind this file is pinned: boot used
// to read `save` in `setMode` one line before `const save` initialized, and
// the only witness was a rejected boot promise reporting into `#notice`,
// which `wiki.html` does not contain. Here the catcher's element is a stub
// like every other, so the silence is assertable.

import test from "node:test";
import assert from "node:assert/strict";

/** A minimal element: exactly what `pages.wiki` and `mountNodes` touch. */
function fakeElement(name) {
  return {
    name,
    textContent: "",
    value: "",
    hidden: false,
    dataset: {},
    attrs: {},
    children: [],
    listeners: {},
    setAttribute(attr, value) {
      this.attrs[attr] = String(value);
    },
    appendChild(child) {
      this.children.push(child);
      return child;
    },
    replaceChildren() {
      this.children = [];
    },
    addEventListener(type, handler) {
      (this.listeners[type] = this.listeners[type] || []).push(handler);
    },
    /** Runs the wired handlers, awaiting the async ones (Save, Edit). */
    click(...args) {
      return Promise.all(
        (this.listeners.click || []).map((handler) => handler(...args)),
      );
    },
  };
}

const byId = new Map();
const element = (id) => {
  if (!byId.has(id)) byId.set(id, fakeElement(id));
  return byId.get(id);
};

globalThis.document = {
  readyState: "complete",
  body: { dataset: { page: "wiki" } },
  documentElement: { dataset: {} },
  querySelector: (sel) => (sel.startsWith("#") ? element(sel.slice(1)) : null),
  querySelectorAll: () => [],
  createElement: (tag) => fakeElement(tag),
  createTextNode: (value) => ({ text: String(value) }),
  createDocumentFragment: () => fakeElement("#fragment"),
  addEventListener() {},
  dispatchEvent: () => true,
};

globalThis.window = {
  matchMedia: () => ({ matches: false }),
  prompt: () => null,
};

// The page the boot-time GET below loads, in the shape the server answers.
const HELLO = {
  slug: "hello",
  title: "Hello",
  markdown: "# Hello\n\nWorld.",
  url: "/pages/hello",
  version: 3,
  entity_type: "note",
  backlinks: ["welcome"],
  citations: [{ title: "A source", url: "https://example.test/a" }],
};

/** The canned API answers; each test swaps `respond` for its own. */
const calls = [];
let respond = (method) => (method === "GET" ? json(HELLO) : json({}, 404));

const json = (body, status = 200) =>
  new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });

globalThis.location = { pathname: "/wiki.html", search: "?slug=hello" };

const replacedUrls = [];
globalThis.history = {
  replaceState(state, unused, url) {
    replacedUrls.push(String(url));
  },
};

globalThis.fetch = async (url, init) => {
  const method = (init && init.method) || "GET";
  calls.push({ method, url: String(url), init });
  return respond(method, String(url), init);
};

// Importing the module runs `boot()`, which starts `pages.wiki()` — a promise
// nobody outside awaits — so drain the queue before asserting on its result.
const settle = () => new Promise((resolve) => setImmediate(resolve));
await import("../assets/app.js");
await settle();
await settle();

const { ApiError } = globalThis.LB;

test("pages.wiki boots past the save-before-initialization crash", async () => {
  // The crash rejected boot into `#notice` (wiki.html has `#wiki-notice`, so
  // the user saw nothing) and stopped the page before `load()` ever ran.
  assert.equal(element("notice").textContent, "", "boot's catcher stays silent");
  assert.equal(element("wiki-notice").textContent, "");

  // `load()` ran: the ?slug= page was fetched and rendered.
  assert.ok(
    calls.some((call) => call.method === "GET" && call.url === "/v1/pages/hello"),
  );
  assert.equal(element("page-title").textContent, "Hello");
  assert.equal(element("source").value, HELLO.markdown);

  // `setMode("view")` ran to completion: view mode is up, edit mode is hidden.
  assert.equal(element("editor-wrap").hidden, true);
  assert.equal(element("view-wrap").hidden, false);
  assert.equal(element("save").hidden, true);
  assert.equal(element("mode-toggle").textContent, "Edit");

  // And the wiring that follows it in the file happened.
  assert.equal(typeof element("save").listeners.click[0], "function");
  assert.equal(typeof element("new-page").listeners.click[0], "function");
});

test("New page: an empty or cancelled prompt changes nothing", async () => {
  const callsBefore = calls.length;
  const urlsBefore = replacedUrls.length;

  globalThis.window.prompt = () => null;
  await element("new-page").click();
  globalThis.window.prompt = () => "   ";
  await element("new-page").click();

  assert.equal(calls.length, callsBefore, "no request on a blank prompt");
  assert.equal(replacedUrls.length, urlsBefore, "no navigation on a blank prompt");
  assert.equal(element("editor-wrap").hidden, true, "still in view mode");
});

test("New page: a slug that fails the shape check is stopped with a notice", async () => {
  const callsBefore = calls.length;
  const urlsBefore = replacedUrls.length;

  globalThis.window.prompt = () => "Hello World!";
  await element("new-page").click();

  assert.match(element("wiki-status").textContent, /letters, digits, hyphens/);
  assert.equal(element("wiki-notice").dataset.tone, "warn");
  assert.equal(calls.length, callsBefore, "no request for a malformed slug");
  assert.equal(replacedUrls.length, urlsBefore, "no navigation for a malformed slug");
  assert.equal(element("editor-wrap").hidden, true, "the editor was not opened");
});

test("New page: the stub opens the editor and Save creates with base_version null", async () => {
  // Trimmed and lowercased, as a slug should arrive at the server.
  globalThis.window.prompt = () => "  API-Notes  ";
  await element("new-page").click();

  // Straight into the editor, no GET. (Boot also fires the header's session
  // check, so count the page calls, not every call.)
  assert.equal(
    calls.filter((call) => call.url.startsWith("/v1/pages/")).length,
    1,
    "only the boot GET has happened so far",
  );
  assert.deepEqual(
    replacedUrls[replacedUrls.length - 1],
    "/wiki.html?slug=api-notes",
    "the location now names the page being created",
  );
  assert.equal(element("page-title").textContent, "api-notes");
  assert.equal(element("editor-wrap").hidden, false);
  assert.equal(element("view-wrap").hidden, true);
  assert.equal(element("save").hidden, false);
  assert.equal(element("mode-toggle").textContent, "Done editing");

  // Write something and save: the PUT must carry `base_version: null`, the
  // server's create signal.
  const source = element("source");
  source.value = "# Fresh page";
  source.listeners.input[0]();

  respond = (method, url) =>
    method === "PUT" && url === "/v1/pages/api-notes"
      ? json({
          slug: "api-notes",
          title: "",
          markdown: "# Fresh page",
          url: "/pages/api-notes",
          version: 1,
          entity_type: "page",
          backlinks: [],
          citations: [],
        })
      : json({}, 404);
  await element("save").click();

  const put = calls[calls.length - 1];
  assert.equal(put.method, "PUT");
  assert.equal(put.url, "/v1/pages/api-notes");
  assert.deepEqual(JSON.parse(put.init.body), {
    markdown: "# Fresh page",
    base_version: null,
    title: "",
  });

  // A clean save lands back in view mode.
  assert.equal(element("wiki-status").textContent, "Saved.");
  assert.equal(element("editor-wrap").hidden, true);
  assert.equal(element("save").hidden, true);
});

test("a 409 keeps the editor open with the draft, and does not throw", async () => {
  // The page created above now exists at version 1; this is an ordinary edit
  // of it that loses the race. Go back into the editor first.
  element("mode-toggle").click();
  assert.equal(element("editor-wrap").hidden, false);

  const source = element("source");
  source.value = "# Fresh page\n\nMy unpublished change.";
  respond = () => json({ title: "stale base_version" }, 409);

  // Awaiting the handler: if it threw, this test fails rather than the
  // rejection vanishing into an unhandled promise.
  await element("save").click();

  const put = calls[calls.length - 1];
  assert.equal(put.method, "PUT");
  assert.equal(put.url, "/v1/pages/api-notes");
  assert.deepEqual(JSON.parse(put.init.body), {
    markdown: "# Fresh page\n\nMy unpublished change.",
    base_version: 1,
    title: "",
  });

  assert.match(element("wiki-status").textContent, /Someone else saved/);
  assert.match(element("wiki-notice").textContent, /Someone else saved/);
  assert.ok(element("wiki-notice").dataset.tone === "bad");
  assert.equal(element("editor-wrap").hidden, false, "the editor stays open");
  assert.equal(
    source.value,
    "# Fresh page\n\nMy unpublished change.",
    "the draft is kept, not overwritten",
  );
  assert.ok(ApiError, "the page distinguishes the API error type");
});
