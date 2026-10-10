// The wiki page, booted for real.
//
// `app.js` imports `wikiPage` from `assets/wiki.js` and dispatches it for
// `body[data-page="wiki"]`, so these tests install the minimal DOM a wiki
// page has — stub elements keyed by id, a `location`, a `history`, a
// `localStorage` and a recorded `fetch` — *before* importing `assets/app.js`.
// The import runs the same `boot()` the browser runs, and every later test
// calls `wikiPage(LB.api)` directly, which is the exact function the page
// runs, with fresh state per test.
//
// Assertions read DOM state — what is `hidden`, what `textContent` a node
// carries, what the recorded `fetch` was sent — never implementation
// internals.

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

/* ------------------------------------------------------------------ stubs */

const store = new Map();
globalThis.localStorage = {
  getItem: (key) => (store.has(key) ? store.get(key) : null),
  setItem: (key, value) => store.set(key, String(value)),
  removeItem: (key) => store.delete(key),
};

const focused = [];
const scrolled = [];

/** A minimal element: exactly what `wikiPage`, `mountNodes` and the session
 * header touch, and nothing more — a method the app never calls cannot pass
 * quietly here, it throws. */
function fakeElement(name) {
  const el = {
    name,
    attrs: {},
    children: [],
    listeners: {},
    dataset: {},
    hidden: false,
    disabled: false,
    open: false,
    value: "",
    className: "",
    _text: "",
    setAttribute(attr, value) {
      this.attrs[attr] = String(value);
    },
    getAttribute(attr) {
      return attr in this.attrs ? this.attrs[attr] : null;
    },
    removeAttribute(attr) {
      delete this.attrs[attr];
    },
    appendChild(child) {
      this.children.push(child);
      if (child && typeof child === "object") child.parentNode = this;
      return child;
    },
    append(...kids) {
      for (const kid of kids) {
        this.children.push(kid);
        if (kid && typeof kid === "object") kid.parentNode = this;
      }
    },
    replaceChildren() {
      this.children = [];
      this._text = "";
    },
    addEventListener(type, handler) {
      (this.listeners[type] = this.listeners[type] || []).push(handler);
    },
    focus() {
      focused.push(this);
    },
    scrollIntoView() {
      scrolled.push(this);
    },
    showModal() {
      this.open = true;
    },
    close() {
      this.open = false;
    },
    // Like a real element: assigned text behaves as one text node, mounted
    // children aggregate, and the two combine.
    get textContent() {
      const kids = this.children
        .map((child) => (child && child.text != null ? child.text : child.textContent))
        .join("");
      return this._text + kids;
    },
    set textContent(value) {
      this._text = String(value == null ? "" : value);
      this.children = [];
    },
    querySelector(sel) {
      return walk(this, sel)[0] || null;
    },
    querySelectorAll(sel) {
      return walk(this, sel);
    },
    async click(...args) {
      for (const handler of this.listeners.click || []) await handler(...args);
    },
    /** Fires a non-click event (`input`, `keydown`, `submit`) at the stub. */
    async fire(type, event = {}) {
      const e = { target: this, preventDefault() {}, ...event };
      for (const handler of this.listeners[type] || []) await handler(e);
    },
  };
  return el;
}

/** Tag / `#id` / `.class` / `[attr]` matching over the stub tree. */
function matchSel(el, sel) {
  if (sel.startsWith("#")) return el.attrs.id === sel.slice(1);
  if (sel.startsWith(".")) {
    return String(el.className || "").split(/\s+/).includes(sel.slice(1));
  }
  if (sel.startsWith("[")) {
    const name = sel.slice(1, sel.indexOf("]"));
    return Object.prototype.hasOwnProperty.call(el.attrs, name);
  }
  return el.name === sel.toLowerCase();
}

function walk(root, sel) {
  const selectors = String(sel)
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
  const out = [];
  const visit = (node) => {
    for (const child of node.children || []) {
      if (child && child.name && selectors.some((s) => matchSel(child, s))) {
        out.push(child);
      }
      visit(child);
    }
  };
  visit(root);
  return out;
}

const byId = new Map();
const el = (id) => {
  if (!byId.has(id)) {
    const e = fakeElement(id);
    e.attrs.id = id;
    byId.set(id, e);
  }
  return byId.get(id);
};

let docListeners = {};

globalThis.document = {
  readyState: "complete",
  body: { dataset: { page: "wiki" } },
  documentElement: { dataset: {} },
  querySelector: (sel) => (sel.startsWith("#") ? el(sel.slice(1)) : null),
  querySelectorAll: (sel) =>
    [...byId.values()].filter((e) =>
      String(sel)
        .split(",")
        .some((s) => matchSel(e, s.trim())),
    ),
  createElement: (tag) => fakeElement(tag),
  // A text node: `nodeType` 3 is what the wikilink TreeWalker looks for, and
  // `replaceWith` is how it swaps one for its text + anchor parts.
  createTextNode: (value) => {
    const node = {
      text: String(value),
      nodeType: 3,
      parentNode: null,
    };
    Object.defineProperty(node, "nodeValue", {
      get() {
        return this.text;
      },
      set(next) {
        this.text = String(next);
      },
    });
    node.replaceWith = (...nodes) => {
      const parent = node.parentNode;
      if (!parent || !Array.isArray(parent.children)) return;
      const at = parent.children.indexOf(node);
      if (at === -1) return;
      parent.children.splice(at, 1, ...nodes);
      for (const kid of nodes) {
        if (kid && typeof kid === "object") kid.parentNode = parent;
      }
    };
    return node;
  },
  // Only text nodes are collected when the SHOW_TEXT bit (`1 << 2`) is set —
  // `whatToShow` is a NodeFilter bitmask, text nodes are `nodeType` 3; the
  // walker recurses through every other child — fragments included.
  createTreeWalker(root, whatToShow) {
    const nodes = [];
    const visit = (parent) => {
      for (const child of parent.children || []) {
        if (child && child.nodeType === 3 && whatToShow & 4) nodes.push(child);
        visit(child);
      }
    };
    visit(root);
    let at = -1;
    return {
      currentNode: root,
      nextNode() {
        at += 1;
        if (at >= nodes.length) return false;
        this.currentNode = nodes[at];
        return true;
      },
    };
  },
  createDocumentFragment: () => fakeElement("#fragment"),
  addEventListener(type, handler) {
    (docListeners[type] = docListeners[type] || []).push(handler);
  },
  dispatchEvent: () => true,
};

globalThis.window = { matchMedia: () => ({ matches: false }) };

const assigns = [];
const replacedUrls = [];
globalThis.location = {
  pathname: "/wiki.html",
  search: "?slug=hello",
  origin: "https://wiki.test",
  assign(url) {
    assigns.push(String(url));
  },
};
globalThis.history = {
  replaceState(state, unused, url) {
    replacedUrls.push(String(url));
  },
};

/* ----------------------------------------------------- canned API answers */

const ME = {
  workspace: { id: "ws_01", name: "Acme" },
  member: { user_id: "usr_01", name: "ada" },
  is_owner: true,
};

const HELLO = {
  slug: "hello",
  title: "Hello",
  markdown: "# Hello\n\nWorld.",
  url: "https://livingbrain.wiki/brain/shared/hello",
  version: 3,
  entity_type: "note",
  backlinks: ["welcome"],
  citations: [{ title: "A source", url: "https://example.test/a" }],
};

const PAGES = {
  pages: [
    {
      scope: "shared",
      slug: "kestrel-routes",
      entity_type: "system",
      version: 4,
      updated_at: "2026-10-01T10:00:00Z",
      url: "https://livingbrain.wiki/brain/shared/kestrel-routes",
    },
    {
      scope: "shared",
      slug: "ada",
      entity_type: "person",
      version: 2,
      updated_at: "2026-10-02T10:00:00Z",
      url: "https://livingbrain.wiki/brain/shared/ada",
    },
  ],
};

const json = (body, status = 200) =>
  new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });

const calls = [];
let respond = (method, url) => {
  if (url === "/v1/workspaces/me") return json(ME);
  if (url === "/v1/pages?limit=200") return json(PAGES);
  if (url === "/v1/pages/hello") return json(HELLO);
  return json({ title: "not found" }, 404);
};

globalThis.fetch = async (url, init) => {
  const method = (init && init.method) || "GET";
  calls.push({ method, url: String(url), init });
  return respond(method, String(url), init);
};

/* --------------------------------------------------------------- plumbing */

const tick = () => new Promise((resolve) => setImmediate(resolve));
const drain = async () => {
  for (let i = 0; i < 10; i++) await tick();
};
const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** Fresh page state for one test: elements, storage, records, location. */
function reset({ search = "" } = {}) {
  byId.clear();
  store.clear();
  calls.length = 0;
  assigns.length = 0;
  replacedUrls.length = 0;
  focused.length = 0;
  scrolled.length = 0;
  docListeners = {};
  backlinksCard = null;
  citationsCard = null;
  globalThis.location.search = search;
}

/** The triggers the page's static HTML carries; the stub DOM needs them
 * pre-tagged for `$$("[data-new-page]")` to find — the home toolbar's, the
 * empty state's and the page-view toolbar's. */
function seedTriggers() {
  el("new-page").setAttribute("data-new-page", "");
  el("empty-new-page").setAttribute("data-new-page", "");
  el("page-new-page").setAttribute("data-new-page", "");
}

/** The page view's two aside cards: wiki.html wraps `#page-backlinks` and
 * `#page-citations` in plain `section.card`s of their own, ids and all
 * absent, so a test that watches a card stand down parents the stub nodes
 * the way the markup does. */
let backlinksCard = null;
let citationsCard = null;
function seedPageCards() {
  backlinksCard = fakeElement("section");
  backlinksCard.className = "card";
  citationsCard = fakeElement("section");
  citationsCard.className = "card";
  el("page-backlinks").parentNode = backlinksCard;
  el("page-citations").parentNode = citationsCard;
}

// Importing the module runs `boot()`, which starts `pages.wiki()` — a promise
// nobody outside awaits — so drain the queue before asserting on its result.
await import("../assets/app.js");
await drain();

const { ApiError } = globalThis.LB;
const wikiPage = globalThis.LB ? (await import("../assets/wiki.js")).wikiPage : null;
const { slugify, parseBrainUrl, validSlug, highlightTerms, stripFrontmatter } =
  await import("../assets/wiki.js");

/* ------------------------------------------------------------------ tests */

test("boot: ?slug=hello lands on the read view, past every stub the page lacks", async () => {
  // boot's catcher reports into `#notice`, which wiki.html does not contain;
  // its staying empty means nothing threw on the way up.
  assert.equal(el("notice").textContent, "");
  assert.ok(
    calls.some((call) => call.method === "GET" && call.url === "/v1/pages/hello"),
    "the page was fetched",
  );
  assert.ok(
    calls.some((call) => call.url === "/v1/workspaces/me"),
    "the session was checked",
  );
  assert.equal(el("page-title").textContent, "Hello");
  assert.equal(el("page-view").hidden, false);
  assert.equal(el("home-view").hidden, true);
  assert.equal(el("editor-view").hidden, true);
  assert.equal(el("new-page-view").hidden, true);
  // Signed in: the header bar and the Edit button are live.
  assert.equal(el("session-bar").hidden, false);
  assert.equal(el("session-who").textContent, "ada");
  assert.equal(el("edit").hidden, false);
  // One heading only: no TOC yet.
  assert.equal(el("toc-desktop").hidden, true);
});

test("pure helpers: slugify, validSlug, parseBrainUrl, highlightTerms", async () => {
  assert.equal(slugify("Kestrel Routes!"), "kestrel-routes");
  assert.equal(slugify("  --mixed  CASE  words--  "), "mixed-case-words");
  assert.equal(slugify("don't"), "dont");
  assert.equal(slugify(""), "");
  assert.ok(slugify("a".repeat(300)).length <= 128);

  assert.equal(validSlug("kestrel-routes"), true);
  assert.equal(validSlug("a"), true);
  assert.equal(validSlug("Kestrel"), false);
  assert.equal(validSlug("has spaces"), false);
  assert.equal(validSlug(""), false);
  assert.equal(validSlug("x".repeat(129)), false);
  assert.equal(validSlug(null), false);

  assert.deepEqual(parseBrainUrl("https://livingbrain.wiki/brain/shared/kestrel-routes"), {
    scope: "shared",
    slug: "kestrel-routes",
  });
  assert.deepEqual(parseBrainUrl("/brain/personal/ada"), { scope: "personal", slug: "ada" });
  assert.equal(parseBrainUrl("https://livingbrain.wiki/brain/solo"), null);
  assert.equal(parseBrainUrl("https://livingbrain.wiki/"), null);
  assert.equal(parseBrainUrl(null), null);

  const parts = highlightTerms("Kestrel flies. Kestrel soars.", ["kestrel"]);
  assert.deepEqual(
    parts.map((p) => (p.tag === "mark" ? `<mark>${p.children[0].value}</mark>` : p.value)),
    ["<mark>Kestrel</mark>", " flies. ", "<mark>Kestrel</mark>", " soars."],
  );
  // Longest term wins when two match at the same index; short terms are ignored.
  const two = highlightTerms("router", ["route", "router", "a"]);
  assert.equal(two[0].tag, "mark");
  assert.equal(two[0].children[0].value, "router");
  // No match: one text part, verbatim.
  assert.deepEqual(highlightTerms("plain", ["zzz"]), [{ type: "text", value: "plain" }]);
});

test("pure helper: stripFrontmatter returns the body after a leading fence", () => {
  // Fence present: everything between the fences goes, one leading newline
  // is trimmed, the body starts clean.
  assert.equal(
    stripFrontmatter("---\ntitle: Launch checklist\n---\n\n# Launch checklist\n\nBody."),
    "# Launch checklist\n\nBody.",
  );
  // No fence at the very start — including a `---` that is only a thematic
  // break further down — and the input comes back unchanged.
  assert.equal(
    stripFrontmatter("# No fence\n\n---\n\njust a rule"),
    "# No fence\n\n---\n\njust a rule",
  );
  // An unclosed fence is not frontmatter; it stays verbatim.
  assert.equal(
    stripFrontmatter("---\ntitle: never closed"),
    "---\ntitle: never closed",
  );
  // Extra fields ride along under the opening fence; no blank line after the
  // closing fence means nothing to trim.
  assert.equal(
    stripFrontmatter("---\ntitle: X\nscope: shared\n---\nbody"),
    "body",
  );
  // A fence-only document strips to nothing.
  assert.equal(stripFrontmatter("---\ntitle: X\n---"), "");
  // The empty string has no fence; it stays empty.
  assert.equal(stripFrontmatter(""), "");
});

test("signed-in home: recent and pinned from /v1/pages, pins from localStorage", async () => {
  reset();
  seedTriggers();
  store.set("lb-wiki-pinned", JSON.stringify(["shared/ada"]));
  await wikiPage(globalThis.LB.api);
  await drain();

  assert.ok(
    calls.some((call) => call.url === "/v1/pages?limit=200"),
    "the list was fetched with the documented limit",
  );
  assert.equal(el("home-view").hidden, false);
  assert.equal(el("page-view").hidden, true);
  assert.equal(el("home-empty").hidden, true, "two pages: no empty state");
  assert.equal(el("home-status").textContent, "2 pages");
  // Signed in: every New page affordance is live.
  assert.equal(el("new-page").hidden, false);
  assert.equal(el("empty-new-page").hidden, false);
  assert.equal(el("page-new-page").hidden, false);

  // Pinned first, the rest in server order.
  const pinnedLinks = walk(el("pinned-list"), "a");
  const recentLinks = walk(el("recent-list"), "a");
  assert.deepEqual(
    pinnedLinks.map((a) => a.href),
    ["wiki.html?slug=ada"],
  );
  assert.deepEqual(
    recentLinks.map((a) => a.href),
    ["wiki.html?slug=kestrel-routes"],
  );
  assert.equal(el("pinned-section").hidden, false);
  assert.equal(el("recent-section").hidden, false);
  assert.match(walk(el("pinned-list"), ".pill")[0].textContent, /person/);

  // Unpinning removes the key and re-renders.
  const unpin = walk(el("pinned-list"), "button")[0];
  assert.match(unpin.textContent, /Unpin/);
  await unpin.click();
  assert.deepEqual(JSON.parse(store.get("lb-wiki-pinned")), []);
  assert.equal(el("pinned-section").hidden, true, "nothing pinned any more");
  assert.equal(walk(el("recent-list"), "a").length, 2, "ada moved back to recent");
});

test("home filter: narrows rows, matches type, says when nothing matches", async () => {
  reset();
  seedTriggers();
  await wikiPage(globalThis.LB.api);
  await drain();

  assert.equal(el("list-filter").hidden, false);
  el("list-filter").value = "kestrel";
  await el("list-filter").fire("input");
  assert.equal(walk(el("recent-list"), "a").length, 1);
  assert.equal(el("recent-section").hidden, false);
  assert.equal(el("home-nomatch").hidden, true);

  el("list-filter").value = "person";
  await el("list-filter").fire("input");
  assert.deepEqual(
    walk(el("recent-list"), "a").map((a) => a.textContent),
    ["adapersonOct 2, 2026 · v2"],
  );

  el("list-filter").value = "zzz-nothing";
  await el("list-filter").fire("input");
  assert.equal(el("recent-section").hidden, true);
  assert.equal(el("home-nomatch").hidden, false);
  assert.equal(el("home-empty").hidden, true, "a failed filter is not an empty wiki");

  el("list-filter").value = "";
  await el("list-filter").fire("input");
  assert.equal(el("home-nomatch").hidden, true);
  assert.equal(walk(el("recent-list"), "a").length, 2);
});

test("home empty state: zero pages explains where pages come from", async () => {
  reset();
  seedTriggers();
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url === "/v1/pages?limit=200") return json({ pages: [] });
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();

  assert.equal(el("home-empty").hidden, false);
  assert.equal(el("pinned-section").hidden, true);
  assert.equal(el("recent-section").hidden, true);
  assert.equal(el("list-filter").hidden, true, "nothing to filter");
  // The empty state's copy lives in the page's static HTML; the stub carries
  // the wiring, the file carries the words.
  const wikiHtml = readFileSync(new URL("../wiki.html", import.meta.url), "utf8");
  assert.match(wikiHtml, /brain_note/);
  assert.match(wikiHtml, /Slack/);
  assert.equal(el("empty-new-page").listeners.click.length, 1, "its New page button is wired");
});

test("signed out: the wiki says so instead of showing lists or an editor", async () => {
  reset();
  seedTriggers();
  respond = () => json({ status: 401, title: "Bearer token required" }, 401);
  await wikiPage(globalThis.LB.api);
  await drain();

  assert.equal(el("home-view").hidden, false);
  assert.equal(el("pinned-section").hidden, true);
  assert.equal(el("recent-section").hidden, true);
  assert.equal(el("home-empty").hidden, true);
  assert.match(el("wiki-notice").textContent, /Sign in/);
  const link = walk(el("wiki-notice"), "a")[0];
  assert.match(link.href, /index\.html/, "the notice links to sign-in");
  assert.equal(el("header-signin").hidden, false, "the header keeps its Sign in link");
  // New page is a write affordance, like Edit: signed out it hides everywhere.
  assert.equal(el("new-page").hidden, true, "the home toolbar's is hidden");
  assert.equal(el("empty-new-page").hidden, true, "the empty state's is hidden");
  assert.equal(el("page-new-page").hidden, true, "the page toolbar's is hidden");
  // Ask hits the API like Edit does: its buttons hide with the write ones.
  assert.equal(el("ask-open").hidden, true, "the page toolbar's Ask is hidden");
  assert.equal(el("ask-open-edit").hidden, true, "the editor's Ask is hidden");
});

test("signed out on a page: title and sign-in notice only — no article, meta, toolbar or cards", async () => {
  reset({ search: "?slug=hello" });
  seedTriggers();
  seedPageCards();
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json({ title: "Bearer token required" }, 401);
    return json(HELLO);
  };
  await wikiPage(globalThis.LB.api);
  await drain();

  assert.equal(el("page-view").hidden, false);
  assert.equal(el("page-title").textContent, "hello", "the slug stays as the title");
  assert.equal(el("edit").hidden, true);
  assert.equal(el("pin-toggle").hidden, true);
  assert.equal(el("ask-open").hidden, true, "Ask hits the API like Edit: it hides too");
  assert.equal(el("ask-open-edit").hidden, true);
  assert.equal(el("page-new-page").hidden, true, "New page hides with the write buttons");
  // The shell is gone: no article, no meta line, no empty aside cards.
  assert.equal(el("page-body").hidden, true, "no article shell");
  assert.equal(el("page-body").textContent, "");
  assert.equal(el("page-meta").hidden, true, "no meta line");
  assert.equal(backlinksCard.hidden, true, "the backlinks card stands down");
  assert.equal(citationsCard.hidden, true, "the citations card stands down");
  assert.equal(el("toc-desktop").hidden, true, "no TOC for an empty article");
  assert.match(el("wiki-notice").textContent, /Sign in/);
  assert.equal(walk(el("wiki-notice"), "a").length, 1);
});

test("page view: title, meta, markdown, backlinks, citations, and a TOC from h2/h3", async () => {
  reset({ search: "?slug=hello" });
  seedTriggers();
  seedPageCards();
  const RAW = [
    "---",
    "title: Launch checklist",
    "scope: shared",
    "---",
    "",
    "# Hello",
    "",
    "## Alpha section",
    "",
    "text",
    "",
    "### Beta deep",
    "",
    "more",
    "",
    "## Alpha section",
    "",
    "again",
  ].join("\n");
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url === "/v1/pages/hello") {
      return json({ ...HELLO, entity_type: "system", markdown: RAW });
    }
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();

  assert.equal(el("page-title").textContent, "Hello");
  assert.match(walk(el("page-meta"), ".pill")[0].textContent, /system/);
  assert.match(el("page-meta").textContent, /v3/);
  assert.match(el("page-meta").textContent, /shared/);

  const headings = walk(el("page-body"), "h2, h3").map((h) => ({
    tag: h.name,
    id: h.getAttribute("id"),
    text: h.textContent,
  }));
  assert.deepEqual(headings, [
    { tag: "h2", id: "alpha-section", text: "Alpha section" },
    { tag: "h3", id: "beta-deep", text: "Beta deep" },
    { tag: "h2", id: "alpha-section-2", text: "Alpha section" },
  ]);

  const tocLinks = walk(el("toc-list"), "a");
  assert.deepEqual(
    tocLinks.map((a) => a.getAttribute("href")),
    ["#alpha-section", "#beta-deep", "#alpha-section-2"],
  );
  const drawerLinks = walk(el("toc-drawer"), "a");
  assert.deepEqual(
    drawerLinks.map((a) => a.getAttribute("href")),
    ["#alpha-section", "#beta-deep", "#alpha-section-2"],
    "the drawer mount carries the same TOC",
  );
  assert.equal(el("toc-desktop").hidden, false);
  assert.equal(el("toc-drawer-heading").hidden, false);

  const backlink = walk(el("page-backlinks"), "a")[0];
  assert.equal(backlink.getAttribute("href"), "wiki.html?slug=welcome");
  const citation = walk(el("page-citations"), "a")[0];
  assert.equal(citation.getAttribute("href"), "https://example.test/a");
  // Both lists came back non-empty, so both cards stay up, with Ask.
  assert.equal(backlinksCard.hidden, false, "a backlink keeps its card");
  assert.equal(citationsCard.hidden, false, "a citation keeps its card");
  assert.equal(el("page-body").hidden, false);
  assert.equal(el("ask-open").hidden, false, "signed in, Ask shows");

  assert.equal(el("pin-toggle").getAttribute("aria-pressed"), "false");
  assert.equal(el("pin-toggle").textContent, "Pin");

  // The article is a display surface: the frontmatter fence never reaches it —
  // no literal `title:` line, no rule where a fence was — while the body and
  // its TOC are exactly as before.
  assert.equal(
    el("page-body").textContent.includes("title: Launch checklist"),
    false,
    "the frontmatter title line is not rendered",
  );
  assert.equal(walk(el("page-body"), "hr").length, 0, "no fence rules either");
  assert.match(el("page-body").textContent, /text/);

  // The editor's textarea is a raw-source surface: the full markdown with the
  // fence stays editable and is what Save would send. Its preview strips.
  await el("edit").click();
  assert.equal(el("editor-view").hidden, false);
  assert.equal(el("source").value, RAW, "the textarea holds the raw markdown");
  await el("edit-preview").fire("click");
  assert.equal(el("preview").hidden, false);
  assert.equal(
    el("preview").textContent.includes("title: Launch checklist"),
    false,
    "the preview strips the fence too",
  );
  assert.match(el("preview").textContent, /Alpha section/);
});

test("signed in, empty backlinks/citations: the cards stand down instead of leaving shells", async () => {
  reset({ search: "?slug=hello" });
  seedTriggers();
  seedPageCards();
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url === "/v1/pages/hello") {
      return json({ ...HELLO, backlinks: [], citations: [] });
    }
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();

  assert.equal(el("page-view").hidden, false);
  assert.equal(el("page-body").hidden, false, "the article itself shows");
  assert.equal(backlinksCard.hidden, true, "no backlinks, no card");
  assert.equal(citationsCard.hidden, true, "no citations, no card");
  assert.equal(el("page-backlinks").hidden, true);
  assert.equal(el("page-citations").hidden, true);
  assert.equal(el("ask-open").hidden, false, "Ask is not a write affordance");
});

test("wikilinks: [[slug]] and [[slug|Label]] render as links, invalid targets stay literal", async () => {
  reset({ search: "?slug=hello" });
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url === "/v1/pages/hello") {
      return json({
        ...HELLO,
        markdown:
          "See [[tamar-routes]] and [[orbit window|Orbit Window]].\n" +
          "A bare [[orbit window]] stays text; [[Tamar-Routes]] links lowercased.",
      });
    }
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();

  // A bare slug links as written; an aliased target is slugified and the
  // alias is the label; a bare non-slug (`[[orbit window]]`) never becomes a
  // link, and a bare slug in any casing links lowercased.
  const links = walk(el("page-body"), "a").map((a) => [
    a.getAttribute("href"),
    a.textContent,
  ]);
  assert.deepEqual(links, [
    ["wiki.html?slug=tamar-routes", "tamar-routes"],
    ["wiki.html?slug=orbit-window", "Orbit Window"],
    ["wiki.html?slug=tamar-routes", "tamar-routes"],
  ]);
  // The words around the links survive verbatim — including the brackets of
  // the target that is not a slug.
  assert.equal(
    el("page-body").textContent,
    "See tamar-routes and Orbit Window.\n" +
      "A bare [[orbit window]] stays text; tamar-routes links lowercased.",
  );
});

test("pin toggle: persists scope/slug to localStorage and flips back", async () => {
  reset({ search: "?slug=hello" });
  await wikiPage(globalThis.LB.api);
  await drain();

  await el("pin-toggle").click();
  assert.deepEqual(JSON.parse(store.get("lb-wiki-pinned")), ["shared/hello"]);
  assert.equal(el("pin-toggle").getAttribute("aria-pressed"), "true");
  assert.equal(el("pin-toggle").textContent, "Unpin");

  await el("pin-toggle").click();
  assert.deepEqual(JSON.parse(store.get("lb-wiki-pinned")), []);
  assert.equal(el("pin-toggle").getAttribute("aria-pressed"), "false");
  assert.equal(el("pin-toggle").textContent, "Pin");
});

test("unknown slug: today's 404 copy and a wired New page", async () => {
  reset({ search: "?slug=nope-here" });
  seedTriggers();
  await wikiPage(globalThis.LB.api);
  await drain();

  assert.equal(el("page-view").hidden, false);
  assert.match(
    el("wiki-notice").textContent,
    /No page has this slug yet — the New page button creates it\./,
  );
  assert.equal(el("edit").hidden, true);
  assert.ok(el("new-page").listeners.click.length > 0, "the New page CTA is wired");
  assert.equal(el("new-page").hidden, false, "signed in, the CTA shows");
});

test("editor: mode=edit opens it, save PUTs the exact body and re-renders from the response", async () => {
  reset({ search: "?slug=hello&mode=edit" });
  const SAVED = { ...HELLO, version: 4, markdown: "# Hello\n\nEdited world." };
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (method === "GET" && url === "/v1/pages/hello") return json(HELLO);
    if (method === "PUT" && url === "/v1/pages/hello") return json(SAVED);
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();

  assert.equal(el("editor-view").hidden, false);
  assert.equal(el("source").value, HELLO.markdown);
  assert.equal(el("edit-title").value, "Hello");

  // Write mode first: the textarea shows, the preview hides.
  assert.equal(el("source").hidden, false);
  assert.equal(el("preview").hidden, true);
  el("source").value = "# Hello\n\nEdited world.";
  await el("edit-preview").fire("click");
  assert.equal(el("preview").hidden, false);
  assert.equal(el("source").hidden, true);
  assert.ok(walk(el("preview"), "p").length > 0, "the preview renders the draft");
  await el("edit-write").fire("click");
  assert.equal(el("source").hidden, false);
  assert.equal(el("preview").hidden, true);

  await el("save").click();
  const put = calls.find((call) => call.method === "PUT");
  assert.equal(put.url, "/v1/pages/hello");
  assert.deepEqual(JSON.parse(put.init.body), {
    markdown: "# Hello\n\nEdited world.",
    base_version: 3,
    title: "Hello",
  });

  // Back on the read view, built from what the server returned.
  assert.equal(el("editor-view").hidden, true);
  assert.equal(el("page-view").hidden, false);
  assert.equal(el("page-body").textContent.includes("Edited world."), true);
  assert.equal(el("source").value, SAVED.markdown, "the editor holds the server's text");
  assert.equal(el("wiki-notice").textContent, "Saved.");
  assert.equal(el("wiki-notice").dataset.tone, "ok");
  assert.equal(replacedUrls[replacedUrls.length - 1], "/wiki.html?slug=hello");
});

test("save: markdown the server changed means a redaction notice", async () => {
  reset({ search: "?slug=hello&mode=edit" });
  const REDACTED = { ...HELLO, version: 4, markdown: "call [REDACTED:Email] now" };
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (method === "GET" && url === "/v1/pages/hello") return json(HELLO);
    if (method === "PUT" && url === "/v1/pages/hello") return json(REDACTED);
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();

  el("source").value = "call ada@example.test now";
  await el("save").click();
  assert.equal(el("wiki-notice").dataset.tone, "warn");
  assert.match(
    el("wiki-notice").textContent,
    /Sensitive-looking text was redacted before saving\./,
  );
  assert.equal(el("page-view").hidden, false);
  assert.equal(el("source").value, REDACTED.markdown, "the response is the truth");
});

test("409: the draft is kept, and Overwrite with mine re-PUTs with the fresh base_version", async () => {
  reset({ search: "?slug=hello" });
  let puts = 0;
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (method === "GET" && url === "/v1/pages/hello") {
      return json(puts ? { ...HELLO, version: 7, title: "Hello" } : HELLO);
    }
    if (method === "PUT" && url === "/v1/pages/hello") {
      puts += 1;
      if (puts === 1) return json({ title: "stale base_version" }, 409);
      return json({ ...HELLO, version: 8, markdown: "# Hello\n\nMy unpublished change." });
    }
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();

  await el("edit").click();
  assert.equal(el("editor-view").hidden, false);
  el("source").value = "# Hello\n\nMy unpublished change.";
  await el("save").click();

  assert.match(el("wiki-notice").textContent, /Someone else saved this page/);
  assert.equal(el("wiki-notice").dataset.tone, "bad");
  assert.equal(el("editor-view").hidden, false, "the editor stays open");
  assert.equal(
    el("source").value,
    "# Hello\n\nMy unpublished change.",
    "the draft is kept, not overwritten",
  );
  const buttons = walk(el("wiki-notice"), "button").map((b) => b.textContent);
  assert.deepEqual(buttons, ["Load their version", "Overwrite with mine"]);

  await walk(el("wiki-notice"), "button")
    .find((b) => b.textContent === "Overwrite with mine")
    .click();
  const putsSent = calls.filter((call) => call.method === "PUT");
  assert.equal(putsSent.length, 2);
  assert.deepEqual(JSON.parse(putsSent[1].init.body), {
    markdown: "# Hello\n\nMy unpublished change.",
    base_version: 7,
    title: "Hello",
    }, "the second PUT carries the fresh base_version");
  assert.equal(el("page-view").hidden, false);
  assert.equal(el("wiki-notice").textContent, "Saved.");
});

test("409: Load their version replaces the draft and stays in the editor", async () => {
  reset({ search: "?slug=hello" });
  let puts = 0;
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (method === "GET" && url === "/v1/pages/hello") {
      return json(puts ? { ...HELLO, version: 9, markdown: "# Their version" } : HELLO);
    }
    if (method === "PUT" && url === "/v1/pages/hello") {
      puts += 1;
      return json({ title: "stale base_version" }, 409);
    }
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();

  await el("edit").click();
  el("source").value = "# My draft";
  await el("save").click();
  await walk(el("wiki-notice"), "button")
    .find((b) => b.textContent === "Load their version")
    .click();

  assert.equal(el("editor-view").hidden, false, "still editing");
  assert.equal(el("source").value, "# Their version", "the draft was replaced");
  assert.match(
    el("wiki-notice").textContent,
    /Loaded the current version — your draft was replaced\./,
  );
});

test("new page: inline slug validation, no fetch while invalid, create PUTs base_version null", async () => {
  reset();
  seedTriggers();
  const created = {
    slug: "api-notes",
    title: "API notes",
    markdown: "",
    url: "https://livingbrain.wiki/brain/shared/api-notes",
    version: 1,
    entity_type: "page",
    backlinks: [],
    citations: [],
  };
  respond = (method, url, init) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url === "/v1/pages?limit=200") return json(PAGES);
    if (method === "PUT" && url === "/v1/pages/api-notes") return json(created);
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();

  await el("new-page").click();
  assert.equal(el("new-page-view").hidden, false);
  assert.equal(el("home-view").hidden, true);
  assert.equal(el("np-create").disabled, true, "an empty slug is invalid");
  assert.match(el("np-slug-error").textContent, /lowercase letters, digits and hyphens/);
  // Pristine: the rule is helper text — no `aria-invalid`, so wiki.css keeps
  // it in the muted helper ink rather than the danger color.
  assert.equal(
    el("np-slug").getAttribute("aria-invalid"),
    null,
    "a pristine form does not wear the error state",
  );

  const pagesCalls = () => calls.filter((call) => call.url.startsWith("/v1/pages/")).length;
  const before = pagesCalls();

  el("np-slug").value = "Bad Slug!";
  await el("np-slug").fire("input");
  assert.match(
    el("np-slug-error").textContent,
    /Use lowercase letters, digits and hyphens — 1 to 128 characters\./,
  );
  assert.equal(el("np-create").disabled, true);
  // A slug was typed and refused: now the slot is the error, and wiki.css
  // turns it danger-red off this attribute.
  assert.equal(
    el("np-slug").getAttribute("aria-invalid"),
    "true",
    "a typed invalid slug is the error state",
  );
  await el("np-form").fire("submit");
  assert.equal(pagesCalls(), before, "an invalid slug never reaches the server");

  el("np-slug").value = "api-notes";
  await el("np-slug").fire("input");
  assert.equal(el("np-slug-error").textContent, "");
  assert.equal(el("np-create").disabled, false);
  assert.equal(el("np-slug").getAttribute("aria-invalid"), null, "valid again, error off");
  el("np-title").value = "API notes";
  await el("np-form").fire("submit");

  const put = calls.find((call) => call.method === "PUT");
  assert.equal(put.url, "/v1/pages/api-notes");
  assert.deepEqual(JSON.parse(put.init.body), {
    markdown: "",
    base_version: null,
    title: "API notes",
  });
  assert.equal(replacedUrls[replacedUrls.length - 1], "/wiki.html?slug=api-notes");
  assert.equal(el("page-view").hidden, false);
  assert.equal(el("page-title").textContent, "API notes");
  assert.equal(el("wiki-notice").textContent, "Created.");
});

test("new page: the server's refusal lands inline on the slug field", async () => {
  reset();
  seedTriggers();
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (method === "PUT" && url === "/v1/pages/bad") {
      return json(
        { type: "…api/page-refused", status: 422, title: "page refused", detail: "reserved word" },
        422,
      );
    }
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();

  await el("new-page").click();
  el("np-slug").value = "bad";
  await el("np-slug").fire("input");
  await el("np-form").fire("submit");
  assert.equal(el("np-slug-error").textContent, "reserved word", "the server's detail, inline");
  assert.equal(
    el("np-slug").getAttribute("aria-invalid"),
    "true",
    "a 422 is a failed validation too: the slot goes danger-red",
  );
  assert.equal(el("new-page-view").hidden, false);
});

test("ask: prefilled question, POST /v1/ask, answer and citations rendered", async () => {
  reset({ search: "?slug=hello" });
  el("ask-submit").textContent = "Ask"; // the label the static HTML carries
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (method === "GET" && url === "/v1/pages/hello") return json(HELLO);
    if (method === "POST" && url === "/v1/ask") {
      return json({
        answer: "Kestrel **flies**.",
        citations: [{ title: "Docs", url: "https://example.test/d", quote: "It flies." }],
      });
    }
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();

  await el("ask-open").click();
  assert.equal(el("ask-panel").open, true, "the dialog is open");
  assert.equal(el("ask-question").value, 'What is "Hello" about?');

  await el("ask-submit").click();
  const post = calls.find((call) => call.method === "POST");
  assert.equal(post.url, "/v1/ask");
  assert.deepEqual(JSON.parse(post.init.body), {
    question: 'What is "Hello" about?',
  });
  assert.ok(walk(el("ask-answer"), "strong").length > 0, "the answer went through the renderer");
  const citation = walk(el("ask-citations"), "a")[0];
  assert.equal(citation.getAttribute("href"), "https://example.test/d");
  assert.equal(walk(el("ask-citations"), "blockquote").length, 1);
  assert.equal(el("ask-submit").disabled, false, "the button came back");
  assert.equal(el("ask-submit").textContent, "Ask");

  // An unsafe citation URL keeps its title but loses its link.
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (method === "POST" && url === "/v1/ask") {
      return json({ answer: "x", citations: [{ title: "Evil", url: "javascript:alert(1)" }] });
    }
    return json({ title: "not found" }, 404);
  };
  await el("ask-submit").click();
  assert.equal(walk(el("ask-citations"), "a").length, 0);
  assert.match(walk(el("ask-citations"), "span")[0].textContent, /Evil/);
});

test("search: debounced, highlighted, keyboard-driven, and quiet below two characters", async () => {
  reset();
  seedTriggers();
  const RESULTS = {
    results: [
      {
        title: "Kestrel routes",
        url: "https://livingbrain.wiki/brain/shared/kestrel-routes",
        snippet: "Kestrel routes edge traffic",
      },
    ],
  };
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url === "/v1/pages?limit=200") return json(PAGES);
    if (url.startsWith("/v1/search?")) return json(RESULTS);
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();

  // One character: no request, no panel.
  el("wiki-search").value = "k";
  await el("wiki-search").fire("input");
  await wait(260);
  assert.equal(
    calls.some((call) => call.url.startsWith("/v1/search")),
    false,
    "below two characters nothing is sent",
  );
  assert.equal(el("wiki-search-results").hidden, true);

  el("wiki-search").value = "kestrel";
  await el("wiki-search").fire("input");
  await wait(260);
  await drain();
  assert.ok(
    calls.some((call) => call.url === "/v1/search?q=kestrel&limit=10"),
    "the debounced search ran with the documented limit",
  );
  assert.equal(el("wiki-search-results").hidden, false);
  const option = el("wiki-search-results").children[0];
  assert.equal(option.getAttribute("role"), "option");
  assert.equal(option.getAttribute("id"), "wiki-search-opt-0");
  const marks = walk(option, "mark").map((m) => m.textContent);
  assert.deepEqual(marks, ["Kestrel", "Kestrel"], "title first-match + every snippet hit");

  // Keyboard: ArrowDown activates the first option, Enter follows it.
  await el("wiki-search").fire("keydown", { key: "ArrowDown" });
  assert.match(option.className, /wiki-search__opt--active/);
  assert.equal(el("wiki-search").getAttribute("aria-activedescendant"), "wiki-search-opt-0");
  await el("wiki-search").fire("keydown", { key: "Enter" });
  assert.deepEqual(assigns, ["wiki.html?slug=kestrel-routes"]);
  assert.equal(el("wiki-search-results").hidden, true, "the panel closed on choose");

  // No results: a line, not silence.
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url.startsWith("/v1/search?")) return json({ results: [] });
    return json({ title: "not found" }, 404);
  };
  el("wiki-search").value = "nothing";
  await el("wiki-search").fire("input");
  await wait(260);
  await drain();
  assert.equal(el("wiki-search-results").hidden, false);
  assert.match(el("wiki-search-results").textContent, /No results\./);

  // Escape closes and clears.
  await el("wiki-search").fire("keydown", { key: "Escape" });
  assert.equal(el("wiki-search-results").hidden, true);
});

test("search: the 401 signed-out state says sign in, and / focuses the box", async () => {
  reset();
  seedTriggers();
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url === "/v1/pages?limit=200") return json(PAGES);
    if (url.startsWith("/v1/search?")) return json({ status: 401, title: "Bearer token required" }, 401);
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();

  el("wiki-search").value = "kestrel";
  await el("wiki-search").fire("input");
  await wait(260);
  await drain();
  assert.equal(el("wiki-search-results").hidden, false);
  assert.match(el("wiki-search-results").textContent, /Sign in to search\./);

  // "/" anywhere outside a field focuses the search box.
  const handlers = docListeners.keydown || [];
  assert.ok(handlers.length > 0, "the global key handler is wired");
  for (const handler of handlers) {
    await handler({ key: "/", target: { tagName: "BODY" }, preventDefault() {} });
  }
  assert.ok(focused.includes(el("wiki-search")), "the search box took the focus");

  // ...but not while typing in one.
  focused.length = 0;
  for (const handler of handlers) {
    await handler({ key: "/", target: { tagName: "TEXTAREA" }, preventDefault() {} });
  }
  assert.equal(focused.length, 0, "a field keeps its keystrokes");
});

test("drawer: toggles with aria-expanded, closes on Escape", async () => {
  reset();
  seedTriggers();
  el("wiki-drawer").hidden = true; // the static HTML ships it hidden
  await wikiPage(globalThis.LB.api);
  await drain();

  const link = fakeElement("a");
  el("wiki-drawer").appendChild(link);

  await el("drawer-toggle").click();
  assert.equal(el("wiki-drawer").hidden, false);
  assert.equal(el("drawer-toggle").getAttribute("aria-expanded"), "true");
  assert.ok(focused.includes(link), "the first link took the focus");

  for (const handler of docListeners.keydown || []) {
    await handler({ key: "Escape", target: { tagName: "BODY" }, preventDefault() {} });
  }
  assert.equal(el("wiki-drawer").hidden, true);
  assert.equal(el("drawer-toggle").getAttribute("aria-expanded"), "false");

  await el("drawer-toggle").click();
  assert.equal(el("wiki-drawer").hidden, false);
  await el("drawer-toggle").click();
  assert.equal(el("wiki-drawer").hidden, true);
});

test("signed-in chrome: drawer sign-in hidden, sign-out mirror shown", async () => {
  reset();
  seedTriggers();
  await wikiPage(globalThis.LB.api);
  await drain();
  assert.equal(el("drawer-signin").hidden, true);
  assert.equal(el("drawer-signout").hidden, false);
});

test("ApiError still carries status and message for the wiki's branching", () => {
  const error = new ApiError(409, { title: "conflict" }, "/v1/pages/hello");
  assert.equal(error.status, 409);
  assert.equal(error.message, "conflict");
});
