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
import { formatDay } from "../assets/facts.js";

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
  if (url.startsWith("/v1/pages?limit=200")) return json(PAGES);
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
const {
  slugify,
  parseBrainUrl,
  validSlug,
  highlightTerms,
  stripFrontmatter,
  validProject,
  searchPath,
  sessionsPath,
  sourcePath,
  transcriptHref,
  latestSessions,
  parseSessionRecord,
  sessionStats,
  transcriptNodes,
  codingAgentsPlace,
  codingAgentsTree,
  codingAgentsLabel,
  queuePath,
  queueSummary,
  jobsBySource,
  jobLabel,
  triageSummary,
  askWithRunner,
  subagentHeader,
  subagentStepNodes,
  formatDuration,
  projectSegment,
  folderSummary,
  sessionsHref,
  githubPath,
  githubRef,
  githubCitation,
  validGithubKind,
  projectLabels,
  compiledTitle,
  displayTitle,
  isSessionRow,
  sessionNoteSlug,
  homePagesPath,
  projectHref,
  humanize,
  listWords,
  projectsModel,
  projectSections,
  agentsModel,
} = await import("../assets/wiki.js");

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
  // The form the server cites now: the web app's own page address.
  assert.deepEqual(
    parseBrainUrl("https://app.livingbrain.wiki/wiki?slug=kestrel-routes&scope=user-01HX"),
    { scope: "user-01HX", slug: "kestrel-routes" },
  );
  assert.deepEqual(
    parseBrainUrl("https://app.brain.test/wiki?slug=coding-agents%2Frepo%2Fs1&scope=shared"),
    { scope: "shared", slug: "coding-agents/repo/s1" },
  );
  assert.deepEqual(parseBrainUrl("wiki.html?slug=ada"), { scope: "", slug: "ada" });
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
    calls.some((call) => call.url.startsWith("/v1/pages?limit=200")),
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
    if (url.startsWith("/v1/pages?limit=200")) return json({ pages: [] });
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
    if (url.startsWith("/v1/pages?limit=200")) return json(PAGES);
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
    if (url.startsWith("/v1/pages?limit=200")) return json(PAGES);
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

test("search: the 401 signed-out state says sign in, and / focuses the box on the Ask view", async () => {
  reset();
  seedTriggers();
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url.startsWith("/v1/pages?limit=200")) return json(PAGES);
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

  // On a page, "/" is the shell's (it opens the palette): the wiki's own
  // handler leaves the box alone.
  let handlers = docListeners.keydown || [];
  assert.ok(handlers.length > 0, "the global key handler is wired");
  focused.length = 0;
  for (const handler of handlers) {
    await handler({ key: "/", target: { tagName: "BODY" }, preventDefault() {} });
  }
  assert.ok(!focused.includes(el("wiki-search")), "off the Ask view the box stays put");

  // On the Ask view, "/" anywhere outside a field focuses the box.
  reset({ search: "?view=ask" });
  seedTriggers();
  await wikiPage(globalThis.LB.api);
  await drain();
  handlers = docListeners.keydown || [];
  focused.length = 0;
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

test("ApiError still carries status and message for the wiki's branching", () => {
  const error = new ApiError(409, { title: "conflict" }, "/v1/pages/hello");
  assert.equal(error.status, 409);
  assert.equal(error.message, "conflict");
});

/* ------------------------------------------------- projects and sessions */

const PROJECTS = {
  projects: [
    { project: "demo", sources: 2 },
    { project: "livingbrain", sources: 5 },
  ],
};

const SESSIONS = {
  sources: [
    {
      id: "01J0000000000000000000000B",
      kind: "agent_log",
      scope: "personal",
      path: "claude-code session 3f2a9c1e",
      project: "demo",
      held: false,
      created_at: "2026-10-02T10:00:00Z",
    },
  ],
  next_cursor: "01J0000000000000000000000B",
};

const MORE_SESSIONS = {
  sources: [
    {
      id: "01J0000000000000000000000A",
      kind: "agent_log",
      scope: "shared",
      path: "codex session a1b2c3d4",
      project: "demo",
      held: false,
      created_at: "2026-10-01T10:00:00Z",
    },
  ],
  next_cursor: null,
};

test("pure helpers: the project rule and the two request paths", () => {
  assert.equal(validProject("livingbrain"), true);
  assert.equal(validProject("my-repo.v2_x"), true);
  assert.equal(validProject(""), false);
  assert.equal(validProject("-leading"), false);
  assert.equal(validProject("has space"), false);
  assert.equal(validProject("a".repeat(65)), false);
  assert.equal(validProject(null), false);

  // No project: the search URL is exactly the one it always was.
  assert.equal(searchPath("kestrel", ""), "/v1/search?q=kestrel&limit=10");
  assert.equal(searchPath("kestrel", "demo"), "/v1/search?q=kestrel&limit=10&project=demo");
  // A value outside the rule is dropped, not sent.
  assert.equal(searchPath("kestrel", "../etc"), "/v1/search?q=kestrel&limit=10");

  assert.equal(sessionsPath(), "/v1/sources?kind=agent_log&limit=50");
  assert.equal(
    sessionsPath({ project: "demo", cursor: "01J0000000000000000000000B" }),
    "/v1/sources?kind=agent_log&limit=50&project=demo&cursor=01J0000000000000000000000B",
  );
});

test("search project filter: filled from /v1/sources/projects, sent as &project=", async () => {
  reset();
  seedTriggers();
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url.startsWith("/v1/pages?limit=200")) return json(PAGES);
    if (url === "/v1/sources/projects") return json(PROJECTS);
    if (url.startsWith("/v1/search?")) return json({ results: [] });
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();

  const select = el("wiki-project");
  assert.equal(select.hidden, false, "two projects: the filter shows");
  assert.deepEqual(
    select.children.map((option) => [option.value, option.textContent]),
    [
      ["", "All projects"],
      ["demo", "demo (2)"],
      ["livingbrain", "livingbrain (5)"],
    ],
  );
  assert.equal(select.value, "", "every project until one is chosen");

  el("wiki-search").value = "kestrel";
  await el("wiki-search").fire("input");
  await wait(260);
  await drain();
  assert.ok(calls.some((call) => call.url === "/v1/search?q=kestrel&limit=10"));

  // Choosing a project re-asks the typed query at once, filtered.
  select.value = "livingbrain";
  await select.fire("change");
  await drain();
  assert.ok(
    calls.some((call) => call.url === "/v1/search?q=kestrel&limit=10&project=livingbrain"),
    "the filtered search went out",
  );
});

test("search project filter: no projects, or a failed fetch, hides it", async () => {
  reset();
  seedTriggers();
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url.startsWith("/v1/pages?limit=200")) return json(PAGES);
    if (url === "/v1/sources/projects") return json({ title: "boom" }, 500);
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();
  assert.equal(el("wiki-project").hidden, true);
  assert.equal(el("home-view").hidden, false, "the page is unharmed");
});

test("agent sessions: ?view=sessions&project= lists, pages on, and refilters", async () => {
  reset({ search: "?view=sessions&project=demo" });
  seedTriggers();
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url === "/v1/sources/projects") return json(PROJECTS);
    if (url === "/v1/sources?kind=agent_log&limit=50&project=demo") return json(SESSIONS);
    if (url === "/v1/sources?kind=agent_log&limit=50&project=demo&cursor=01J0000000000000000000000B") {
      return json(MORE_SESSIONS);
    }
    if (url === "/v1/sources?kind=agent_log&limit=50") return json({ sources: [], next_cursor: null });
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();

  assert.equal(el("sessions-view").hidden, false);
  assert.equal(el("home-view").hidden, true);
  assert.equal(el("sessions-project").value, "demo", "the address chose the filter");
  assert.equal(el("sessions-project").hidden, false);
  const rows = () => walk(el("sessions-list"), ".wiki-row__slug").map((n) => n.textContent);
  assert.deepEqual(rows(), ["claude-code session 3f2a9c1e"]);
  assert.match(walk(el("sessions-list"), ".pill")[0].textContent, /demo/);
  assert.equal(el("sessions-more").hidden, false, "a full page has a next one");
  assert.equal(el("sessions-status").textContent, "1+ session");

  await el("sessions-more").click();
  await drain();
  assert.deepEqual(rows(), ["claude-code session 3f2a9c1e", "codex session a1b2c3d4"]);
  assert.equal(el("sessions-more").hidden, true, "the last page");
  assert.equal(el("sessions-status").textContent, "2 sessions");

  // Back to every project: the address follows, the list reloads (empty).
  el("sessions-project").value = "";
  await el("sessions-project").fire("change");
  await drain();
  assert.deepEqual(replacedUrls.slice(-1), ["wiki.html?view=sessions"]);
  assert.ok(calls.some((call) => call.url === "/v1/sources?kind=agent_log&limit=50"));
  assert.equal(el("sessions-list").hidden, true);
  assert.equal(el("sessions-empty").hidden, false, "no sessions explains where they come from");
});

test("agent sessions: signed out says sign in and sends no listing", async () => {
  reset({ search: "?view=sessions" });
  seedTriggers();
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json({ title: "unauthorized" }, 401);
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();
  assert.equal(el("sessions-view").hidden, false);
  assert.match(el("wiki-notice").textContent, /Sign in to see your agent sessions\./);
  assert.equal(
    calls.some((call) => call.url.startsWith("/v1/sources")),
    false,
    "nothing about sources is asked signed out",
  );
});

/* ---------------------------------------------------------- transcripts */

/** A version-2 session record: messages, reasoning, an event, and tool
 * calls with input and output — the shape `livingbrain logs sync` uploads. */
const RECORD = {
  schema_version: 2,
  agent: "codex",
  collector_version: 2,
  detail: "full",
  session_id: "019d1127-b465-7a23-8678-31c5a98a39fe",
  repo: "~/code/demo",
  branch: "main",
  models: ["gpt-5.4"],
  started_at: "2026-10-02T09:00:00.000Z",
  ended_at: "2026-10-02T09:00:10.000Z",
  turns: [
    { role: "user", at: "2026-10-02T09:00:01.000Z", text: "Fix all the TODOs\n<b>not html</b>", tool_calls: [] },
    { role: "reasoning", at: "2026-10-02T09:00:02.000Z", text: "Search first.", tool_calls: [] },
    {
      role: "assistant",
      at: "2026-10-02T09:00:03.000Z",
      text: "",
      tool_calls: [
        { name: "exec_command", format: "shell", input: "rg -n TODO", output: "src/a.rs:3\n", status: "ok", exit_code: 0 },
        { name: "apply_patch", format: "diff", input: "*** Begin Patch\n+// done\n*** End Patch", status: "error", exit_code: 1 },
      ],
    },
    { role: "assistant", at: "2026-10-02T09:00:08.000Z", text: "Done: **one** TODO resolved.", tool_calls: [] },
    { role: "event", at: "2026-10-02T09:00:10.000Z", text: "Turn interrupted by the user", tool_calls: [] },
  ],
  totals: { input_tokens: 1500, output_tokens: 200, cache_read_tokens: 300, cache_creation_tokens: 0, reasoning_tokens: 64, cost_usd: null },
  outcome: null,
  truncated: false,
  omitted_turns: 0,
};

/** Every text leaf under a node description, joined. */
function textOf(nodes) {
  const list = Array.isArray(nodes) ? nodes : [nodes];
  return list
    .map((node) => (node == null ? "" : node.type === "text" ? node.value : textOf(node.children || [])))
    .join("");
}

/** Every element description with `tag`, depth first. */
function tagged(nodes, tag) {
  const out = [];
  const visit = (list) => {
    for (const node of list || []) {
      if (!node || node.type !== "element") continue;
      if (node.tag === tag) out.push(node);
      visit(node.children);
    }
  };
  visit(Array.isArray(nodes) ? nodes : [nodes]);
  return out;
}

test("transcript helpers: paths, newest row per session, record parsing, stats", () => {
  assert.equal(sourcePath("01J0/x"), "/v1/sources/01J0%2Fx");
  assert.equal(transcriptHref("01JB"), "wiki.html?view=sessions&source=01JB");
  assert.deepEqual(
    latestSessions([
      { id: "new", path: "codex session a" },
      { id: "old", path: "codex session a" },
      { id: "b", path: "codex session b" },
    ]).map((row) => row.id),
    ["new", "b"],
  );
  assert.equal(parseSessionRecord("not json"), null);
  assert.equal(parseSessionRecord('{"turns":3}'), null);
  assert.equal(parseSessionRecord(JSON.stringify(RECORD)).agent, "codex");
  assert.deepEqual(sessionStats(RECORD), { user: 1, assistant: 1, tools: 2 });
});

test("transcript nodes: every turn in order, tool calls as blocks, text never markup", () => {
  const nodes = transcriptNodes(RECORD);
  const all = textOf(nodes);
  assert.match(all, /Agent: codex/);
  assert.match(all, /Tool calls: 2/);
  assert.match(all, /Detail: full/);
  // In order: the prompt, the reasoning, both calls, the answer, the event.
  const order = ["Fix all the TODOs", "Search first.", "rg -n TODO", "+// done", "Done: ", "Turn interrupted"];
  let at = -1;
  for (const piece of order) {
    const next = all.indexOf(piece);
    assert.ok(next > at, `${piece} out of order in ${all}`);
    at = next;
  }
  // The person's angle brackets are text, not a tag.
  assert.match(all, /<b>not html<\/b>/);
  assert.equal(tagged(nodes, "b").length, 0);
  // The assistant's markdown is rendered; the calls' inputs are fenced.
  assert.equal(tagged(nodes, "strong").some((n) => textOf(n) === "one"), true);
  const pres = tagged(nodes, "pre");
  assert.equal(pres[0].attrs["data-lang"], "sh");
  assert.equal(pres[2].attrs["data-lang"], "diff");
  // A short output is open; the failed call says so.
  const details = tagged(nodes, "details").filter((n) => n.attrs.class === "transcript-call__output");
  assert.equal(details.length, 1);
  assert.equal("open" in details[0].attrs, true);
  assert.match(all, /error · exit 1/);
});

test("transcript nodes: a version-1 record (names only) still reads", () => {
  const v1 = {
    schema_version: 1,
    agent: "claude_code",
    session_id: "3f2a",
    repo: "~/code/demo",
    models: [],
    turns: [
      { role: "user", text: "hi", tool_calls: [] },
      { role: "assistant", text: "", tool_calls: [{ name: "Bash" }] },
    ],
    totals: {},
    truncated: true,
  };
  const all = textOf(transcriptNodes(v1));
  assert.match(all, /hi/);
  assert.match(all, /Bash/);
  assert.match(all, /summary/);
  assert.match(all, /left out to fit/);
});

test("agent sessions: ?view=sessions&source= opens the transcript from GET /v1/sources/{id}", async () => {
  reset({ search: "?view=sessions&source=01J0000000000000000000000B" });
  seedTriggers();
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url === "/v1/sources/projects") return json(PROJECTS);
    if (url === "/v1/sources/01J0000000000000000000000B") {
      return json({
        id: "01J0000000000000000000000B",
        kind: "agent_log",
        project: "demo",
        body: JSON.stringify(RECORD),
      });
    }
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();
  assert.equal(el("transcript-view").hidden, false);
  assert.equal(el("sessions-view").hidden, true);
  assert.match(el("transcript-title").textContent, /codex session 019d1127 · demo/);
  const body = el("transcript-body").textContent;
  assert.match(body, /rg -n TODO/);
  assert.match(body, /Done: /);
  assert.equal(
    calls.some((call) => call.url === "/v1/sources/01J0000000000000000000000B"),
    true,
  );
});

test("agent sessions: each row links to its transcript, one row per session", async () => {
  reset({ search: "?view=sessions" });
  seedTriggers();
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url === "/v1/sources/projects") return json(PROJECTS);
    if (url === "/v1/sources?kind=agent_log&limit=50") {
      return json({
        sources: [
          { ...SESSIONS.sources[0], id: "01J0000000000000000000000C" },
          SESSIONS.sources[0],
        ],
        next_cursor: null,
      });
    }
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();
  const links = walk(el("sessions-list"), "a");
  assert.equal(links.length, 1, "the older source of the same session is folded away");
  assert.equal(links[0].href, "wiki.html?view=sessions&source=01J0000000000000000000000C");
});

/* ------------------------------------------------- the coding-agents wiki */

const CA_PAGES = {
  pages: [
    { scope: "personal", slug: "coding-agents--index", entity_type: "topic", version: 2, updated_at: "2026-10-15T10:00:00Z" },
    { scope: "personal", slug: "coding-agents--log", entity_type: "topic", version: 3, updated_at: "2026-10-15T10:00:00Z" },
    { scope: "personal", slug: "coding-agents--claude--patterns", entity_type: "topic", version: 1, updated_at: "2026-10-15T10:00:00Z" },
    { scope: "personal", slug: "coding-agents--claude--demo--decisions", entity_type: "topic", version: 1, updated_at: "2026-10-15T10:00:00Z" },
    { scope: "personal", slug: "coding-agents--claude--demo--index", entity_type: "topic", version: 1, updated_at: "2026-10-15T10:00:00Z" },
    { scope: "personal", slug: "coding-agents--codex--demo--learnings", entity_type: "topic", version: 1, updated_at: "2026-10-15T10:00:00Z" },
    { scope: "personal", slug: "coding-agents--projects--demo", entity_type: "topic", version: 1, updated_at: "2026-10-15T10:00:00Z" },
    { scope: "shared", slug: "kestrel-routes", entity_type: "system", version: 4, updated_at: "2026-10-01T10:00:00Z" },
  ],
};

test("coding-agents helpers: a slug splits into its folder, and a listing into a tree", () => {
  assert.deepEqual(codingAgentsPlace("coding-agents--claude--chi-infra--decisions"), {
    kind: "project",
    path: ["coding-agents", "claude", "chi-infra", "decisions"],
    agent: "claude",
    project: "chi-infra",
    page: "decisions",
  });
  assert.equal(codingAgentsPlace("coding-agents--projects--demo").kind, "rollup");
  assert.equal(codingAgentsPlace("coding-agents--claude--patterns").kind, "agent");
  assert.equal(codingAgentsPlace("coding-agents--index").kind, "top");
  assert.equal(codingAgentsPlace("coding-agentsx--index"), null);
  assert.equal(codingAgentsPlace("kestrel-routes"), null);
  assert.equal(codingAgentsPlace("coding-agents---index"), null);
  assert.equal(codingAgentsLabel("coding-agents--codex--demo--learnings"), "coding-agents/codex/demo/learnings");

  const tree = codingAgentsTree(CA_PAGES.pages);
  assert.deepEqual(tree.top.map((e) => e.place.page), ["index", "log"]);
  assert.deepEqual(tree.agents.map((a) => a.agent), ["claude", "codex"]);
  assert.deepEqual(tree.agents[0].pages.map((e) => e.place.page), ["patterns"]);
  assert.deepEqual(tree.agents[0].projects[0].pages.map((e) => e.place.page), ["index", "decisions"]);
  assert.deepEqual(tree.rollups.map((e) => e.place.project), ["demo"]);

  assert.equal(queuePath(), "/v1/coding-agents/queue?limit=100");
  assert.equal(queuePath("01J/x"), "/v1/coding-agents/queue?source=01J%2Fx");
  assert.equal(queueSummary({ queued: 2, done: 5, failed: 0 }), "2 queued · 5 compiled");
  assert.equal(queueSummary({}), "Nothing queued");
  assert.equal(jobLabel("skipped-no-model"), "No model");
  assert.equal(jobLabel("waiting-runner"), "Waiting for your device");
  const jobs = jobsBySource([
    { source_id: "a", status: "done" },
    { source_id: "a", status: "queued" },
  ]);
  assert.equal(jobs.get("a").status, "done", "the newest job per source");
});

test("the queue card says what triage held back and what it saved", () => {
  assert.equal(jobLabel("skipped-trivial"), "Skipped (trivial)");
  assert.equal(jobLabel("summary-only"), "Summary only");
  assert.equal(
    queueSummary({ done: 2, "skipped-trivial": 3, "summary-only": 1 }),
    "2 compiled · 3 skipped (trivial) · 1 summary only",
  );
  assert.equal(
    triageSummary({
      enabled: true,
      classifier: false,
      skipped: 3,
      summarised: 1,
      full: 2,
      tokens_raw: 120_000,
      tokens_sent: 30_000,
      tokens_saved: 90_000,
    }),
    "Triage: 3 skipped · 1 summary only · 2 compiled in full · ~90k tokens saved (75%).",
  );
  assert.match(
    triageSummary({ enabled: true, classifier: true, full: 1, tokens_raw: 10, tokens_saved: 0 }),
    /Triage by Jev\./,
  );
  assert.equal(triageSummary({ enabled: true }), "");
  assert.equal(triageSummary({ enabled: false }), "Triage is off.");
  assert.equal(triageSummary(undefined), "");
});

/** Production's shape: compiled pages of one project from two agents, an
 * agent's own page, one plain page, and a session log (left out of Pages
 * by the server unless asked for). */
const WIKI_PAGES = {
  pages: [
    { scope: "personal", slug: "coding-agents--codex--earthos-world--index", title: "codex / earthos-world: index", entity_type: "topic", version: 1, updated_at: "2026-10-15T12:00:00Z" },
    { scope: "personal", slug: "coding-agents--codex--earthos-world--learnings", title: "codex / earthos-world: learnings", entity_type: "topic", version: 2, updated_at: "2026-10-15T12:00:00Z" },
    { scope: "personal", slug: "coding-agents--claude--earthos-world--learnings", title: "Learnings", entity_type: "topic", version: 1, updated_at: "2026-10-15T11:00:00Z" },
    { scope: "personal", slug: "coding-agents--codex--earthos-world--auth-redirect", title: "The auth redirect loop", entity_type: "topic", version: 1, updated_at: "2026-10-15T11:00:00Z" },
    { scope: "personal", slug: "coding-agents--projects--earthos-world", title: "Project earthos-world: all agents", entity_type: "topic", version: 1, updated_at: "2026-10-15T11:00:00Z" },
    { scope: "personal", slug: "coding-agents--codex--patterns", title: "codex: patterns", entity_type: "topic", version: 1, updated_at: "2026-10-15T11:00:00Z" },
    { scope: "shared", slug: "kestrel-routes", title: "Kestrel routes", entity_type: "system", version: 4, updated_at: "2026-10-01T10:00:00Z" },
  ],
};
const SESSION_PAGES = {
  pages: [
    { scope: "personal", slug: "agent-log-colonizer-a15fba01", title: "EarthOS.world · Colony a15fba01 · #123 Fix auth redirect", entity_type: "session", version: 2, updated_at: "2026-10-15T13:00:00Z" },
  ],
};
const WIKI_PROJECTS = {
  projects: [
    { project: "EarthOS.world", key: "earthos-world", sources: 12 },
    { project: "livingbrain", key: "livingbrain", sources: 3 },
  ],
};

/** Nodes mounted from descriptions carry their class as an attribute:
 * find them by it. */
function byClass(root, cls) {
  const out = [];
  const visit = (node) => {
    for (const child of node.children || []) {
      const names = String((child && child.attrs && child.attrs.class) || (child && child.className) || "");
      if (child && child.name && names.split(/\s+/).includes(cls)) out.push(child);
      if (child) visit(child);
    }
  };
  visit(root);
  return out;
}

function wikiRespond(extra = () => null) {
  return (method, url) => {
    const more = extra(method, url);
    if (more) return more;
    if (url === "/v1/workspaces/me") return json(ME);
    if (url === "/v1/sources/projects") return json(WIKI_PROJECTS);
    if (url === "/v1/pages?limit=200&exclude_type=session") return json(WIKI_PAGES);
    if (url === "/v1/pages?limit=200") {
      return json({ pages: [...SESSION_PAGES.pages, ...WIKI_PAGES.pages] });
    }
    if (url === "/v1/pages?type=session&limit=200") return json(SESSION_PAGES);
    return json({ title: "not found" }, 404);
  };
}

test("pages home: projects first, by their real names; session logs left out by the server", async () => {
  reset({ search: "" });
  seedTriggers();
  respond = wikiRespond();
  await wikiPage(globalThis.LB.api);
  await drain();
  assert.ok(
    calls.some((call) => call.url === "/v1/pages?limit=200&exclude_type=session"),
    "the server leaves the session logs out, before the limit",
  );
  assert.equal(el("projects-section").hidden, false);
  const cards = byClass(el("projects-grid"), "project-card__name").map((n) => n.textContent);
  // Compiled first, by the repository's own spelling, never the folder's.
  assert.deepEqual(cards, ["EarthOS.world", "livingbrain"]);
  const first = walk(el("projects-grid"), "article")[0];
  assert.equal(walk(first, "a")[0].attrs.href, "wiki.html?view=project&project=earthos-world");
  assert.match(first.textContent, /5 compiled pages/);
  assert.match(first.textContent, /From Claude and Codex/);
  const sections = byClass(first, "project-card__sections")[0];
  assert.deepEqual(
    walk(sections, "a").map((a) => a.textContent),
    ["Overview", "Learnings", "1 topic"],
  );
  assert.equal(
    walk(sections, "a")[1].attrs.href,
    "wiki.html?view=project&project=earthos-world&section=learnings",
  );
  // Recent lists the pages that are not compiled, by title; no session log.
  const recent = walk(el("recent-list"), ".wiki-row__slug").map((n) => n.textContent);
  assert.deepEqual(recent, ["Kestrel routes"]);
  // The agents' own pages sit in their own section.
  assert.equal(el("agents-section").hidden, false);
  assert.match(el("agents-list").textContent, /Codex/);
  assert.match(el("agents-list").textContent, /Patterns/);
  // Where the session logs went.
  assert.equal(el("home-ca-hint").hidden, false);
  assert.match(el("home-ca-hint").textContent, /Agent sessions/);
  assert.equal(walk(el("home-ca-hint"), "a")[0].href, "wiki.html?view=sessions");
  assert.equal(el("show-sessions-label").hidden, false);
  assert.equal(el("show-sessions").checked, false);

  // A filter finds a compiled page again, by its human title.
  el("list-filter").value = "learnings";
  await el("list-filter").fire("input");
  const found = walk(el("recent-list"), ".wiki-row__slug").map((n) => n.textContent);
  assert.ok(found.includes("EarthOS.world — Learnings"), String(found));
  assert.ok(!found.some((t) => t.includes("coding-agents--")), String(found));
});

test("pages home: Show session logs asks for them and lists them by title", async () => {
  reset({ search: "" });
  seedTriggers();
  respond = wikiRespond();
  await wikiPage(globalThis.LB.api);
  await drain();
  el("show-sessions").checked = true;
  await el("show-sessions").fire("change");
  await drain();
  assert.ok(calls.some((call) => call.url === "/v1/pages?limit=200"));
  const recent = walk(el("recent-list"), ".wiki-row__slug").map((n) => n.textContent);
  assert.deepEqual(recent, [
    "EarthOS.world · Colony a15fba01 · #123 Fix auth redirect",
    "Kestrel routes",
  ]);
  assert.equal(el("recent-h").textContent, "Recent pages and session logs");
  assert.equal(el("home-ca-hint").hidden, true);
  assert.equal(store.get("lb-wiki-show-sessions"), "1", "remembered per browser");
});

test("project view: one project's wiki from every agent, its sessions, GitHub and facts", async () => {
  reset({ search: "?view=project&project=earthos-world" });
  seedTriggers();
  respond = wikiRespond((method, url) => {
    if (url === "/v1/pages?project=earthos-world&limit=200") {
      return json({ pages: WIKI_PAGES.pages.filter((p) => p.slug.includes("earthos-world")) });
    }
    if (url === "/v1/sources?project=EarthOS.world&limit=50") {
      return json({
        sources: [
          { id: "01S1", kind: "agent_log", path: "colonizer session a15fba01", project: "EarthOS.world", created_at: "2026-10-15T13:00:00Z" },
          { id: "01G1", kind: "github_issue", path: "github/World-360/EarthOS.world/issue/123", project: "EarthOS.world", created_at: "2026-10-14T13:00:00Z" },
        ],
        next_cursor: null,
      });
    }
    if (url === "/v1/facts") {
      return json({
        keys: [
          { subject: "project/earthos-world", attribute: "status", current: [{ value: "beta" }], history: [] },
          { subject: "person/ada", attribute: "role", current: [{ value: "cto" }], history: [] },
        ],
      });
    }
    return null;
  });
  await wikiPage(globalThis.LB.api);
  await drain();
  assert.equal(el("project-view").hidden, false);
  assert.equal(el("home-view").hidden, true);
  assert.equal(el("project-title").textContent, "EarthOS.world");
  assert.deepEqual(
    walk(el("project-crumbs"), "li").map((li) => li.textContent),
    ["Pages", "EarthOS.world"],
  );
  assert.match(el("project-lede").textContent, /Claude and Codex/);
  const body = el("project-body");
  const wiki = walk(body, "section").find((s) => s.attrs.id === "project-wiki");
  const mains = byClass(wiki, "project-list__main").map((n) => n.textContent);
  assert.deepEqual(mains, [
    "EarthOS.world — Overview",
    "EarthOS.world — Learnings",
    "EarthOS.world — All agents",
  ]);
  // Learnings: one section, from both agents, each agent's page one click on.
  const learnings = walk(wiki, "li")[1];
  assert.match(learnings.textContent, /from Claude and Codex/);
  assert.deepEqual(
    walk(learnings, "a").map((a) => a.attrs.href),
    [
      "wiki.html?view=project&project=earthos-world&section=learnings",
      "wiki.html?slug=coding-agents--claude--earthos-world--learnings",
      "wiki.html?slug=coding-agents--codex--earthos-world--learnings",
    ],
  );
  const topics = walk(body, "section").find((s) => s.attrs.id === "project-topics");
  assert.match(topics.textContent, /The auth redirect loop/);
  const sessions = walk(body, "section").find((s) => s.attrs.id === "project-sessions");
  assert.match(sessions.textContent, /EarthOS.world · Colony a15fba01 · #123 Fix auth redirect/);
  assert.equal(walk(sessions, "a")[0].attrs.href, "wiki.html?view=sessions&source=01S1");
  const github = walk(body, "section").find((s) => s.attrs.id === "project-github");
  assert.equal(walk(github, "a")[0].attrs.href, "https://github.com/World-360/EarthOS.world/issues/123");
  const facts = walk(body, "section").find((s) => s.attrs.id === "project-facts");
  assert.match(facts.textContent, /status: beta/);
  assert.doesNotMatch(facts.textContent, /cto/);
});

test("project view: a section reads every agent's page in turn", async () => {
  reset({ search: "?view=project&project=earthos-world&section=learnings" });
  seedTriggers();
  respond = wikiRespond((method, url) => {
    if (url === "/v1/pages?project=earthos-world&limit=200") {
      return json({ pages: WIKI_PAGES.pages.filter((p) => p.slug.includes("earthos-world")) });
    }
    if (url === "/v1/pages/coding-agents--claude--earthos-world--learnings") {
      return json({ slug: "x", markdown: "---\ntitle: Learnings\n---\n\nClaude learned **this**.", title: "Learnings" });
    }
    if (url === "/v1/pages/coding-agents--codex--earthos-world--learnings") {
      return json({ slug: "y", markdown: "Codex learned [[kestrel-routes]].", title: "x" });
    }
    if (url.startsWith("/v1/sources?")) return json({ sources: [] });
    return null;
  });
  await wikiPage(globalThis.LB.api);
  await drain();
  assert.equal(el("project-title").textContent, "EarthOS.world — Learnings");
  assert.equal(el("project-lede").textContent, "From Claude and Codex.");
  assert.deepEqual(
    walk(el("project-crumbs"), "li").map((li) => li.textContent),
    ["Pages", "EarthOS.world", "Learnings"],
  );
  const parts = walk(el("project-body"), "section");
  assert.deepEqual(walk(el("project-body"), "h2").map((h) => h.textContent), ["From Claude", "From Codex"]);
  assert.match(parts[0].textContent, /Claude learned this/);
  assert.doesNotMatch(parts[0].textContent, /title:/, "the frontmatter is not shown");
  // Wiki-links inside a part are real links.
  assert.ok(walk(parts[1], "a").some((a) => a.attrs.href === "wiki.html?slug=kestrel-routes"));
});

test("a compiled page reads by its human title, filed under its project", async () => {
  reset({ search: "?slug=coding-agents--codex--earthos-world--learnings" });
  seedTriggers();
  respond = wikiRespond((method, url) => {
    if (url === "/v1/pages/coding-agents--codex--earthos-world--learnings") {
      return json({
        slug: "coding-agents--codex--earthos-world--learnings",
        title: "codex / earthos-world: learnings",
        markdown: "Body.",
        entity_type: "topic",
        version: 2,
        url: "https://app.test/wiki?slug=coding-agents--codex--earthos-world--learnings&scope=user-1",
        backlinks: [],
        citations: [],
      });
    }
    return null;
  });
  await wikiPage(globalThis.LB.api);
  await drain();
  assert.equal(el("page-title").textContent, "EarthOS.world — Learnings");
  assert.deepEqual(
    walk(el("page-crumbs"), "li").map((li) => li.textContent),
    ["Pages", "EarthOS.world", "Learnings", "Codex"],
  );
  assert.match(el("page-meta").textContent, /from Codex/);
});

test("agent sessions: each session is named by its note's title", async () => {
  reset({ search: "?view=sessions" });
  seedTriggers();
  respond = wikiRespond((method, url) => {
    if (url.startsWith("/v1/sources?kind=agent_log")) {
      return json({
        sources: [{ id: "01S1", kind: "agent_log", path: "colonizer session a15fba01", project: "EarthOS.world", created_at: "2026-10-15T13:00:00Z" }],
        next_cursor: null,
      });
    }
    if (url.startsWith("/v1/coding-agents/queue")) return json({ counts: {}, jobs: [] });
    return null;
  });
  await wikiPage(globalThis.LB.api);
  await drain();
  const names = walk(el("sessions-list"), ".wiki-row__slug").map((n) => n.textContent);
  assert.deepEqual(names, ["EarthOS.world · Colony a15fba01 · #123 Fix auth redirect"]);
});

test("wiki helpers: titles, the project key, the session note slug, the request", () => {
  const labels = projectLabels(WIKI_PROJECTS.projects);
  assert.equal(labels.get("earthos-world"), "EarthOS.world");
  // A folded spelling never wins over the repository's own.
  assert.equal(
    projectLabels([{ project: "EarthOS.world" }, { project: "earthos-world" }]).get("earthos-world"),
    "EarthOS.world",
  );
  const place = (slug) => codingAgentsPlace(slug);
  assert.equal(compiledTitle(place("coding-agents--codex--earthos-world--learnings"), "codex / earthos-world: learnings", labels), "EarthOS.world — Learnings");
  assert.equal(compiledTitle(place("coding-agents--codex--earthos-world--open-questions"), "", labels), "EarthOS.world — Open questions");
  assert.equal(compiledTitle(place("coding-agents--codex--earthos-world--auth-redirect"), "The auth redirect loop", labels), "EarthOS.world — The auth redirect loop");
  assert.equal(compiledTitle(place("coding-agents--codex--earthos-world--auth-redirect"), "codex / earthos-world: auth-redirect", labels), "EarthOS.world — Auth redirect");
  assert.equal(compiledTitle(place("coding-agents--projects--earthos-world"), "", labels), "EarthOS.world — All agents");
  assert.equal(compiledTitle(place("coding-agents--claude--patterns"), "claude: patterns", labels), "Claude — Patterns");
  assert.equal(compiledTitle(place("coding-agents--index"), "", labels), "Coding agents — Overview");
  // An unknown project falls back to its key, never to a slug.
  assert.equal(compiledTitle(place("coding-agents--codex--side--index"), "", labels), "side — Overview");
  assert.equal(displayTitle({ slug: "kestrel" }, labels), "kestrel");
  assert.equal(displayTitle({ slug: "kestrel", title: "Kestrel" }, labels), "Kestrel");
  assert.equal(isSessionRow({ slug: "agent-log-codex-1", entity_type: "decision" }), true);
  assert.equal(isSessionRow({ slug: "kestrel", entity_type: "session" }), true);
  assert.equal(isSessionRow({ slug: "kestrel", entity_type: "system" }), false);
  assert.equal(sessionNoteSlug("colonizer session a15fba01"), "agent-log-colonizer-a15fba01");
  assert.equal(sessionNoteSlug("claude_code session 3F2A_9c"), "agent-log-claude-code-3f2a-9c");
  assert.equal(sessionNoteSlug("notes/one.md"), "");
  assert.equal(homePagesPath(), "/v1/pages?limit=200&exclude_type=session");
  assert.equal(homePagesPath(true), "/v1/pages?limit=200");
  assert.equal(projectHref("earthos-world", "learnings"), "wiki.html?view=project&project=earthos-world&section=learnings");
  assert.equal(humanize("open-questions"), "Open questions");
  assert.equal(listWords(["Claude", "Codex", "Colonizer"]), "Claude, Codex and Colonizer");

  const projects = projectsModel(WIKI_PAGES.pages, WIKI_PROJECTS.projects, {
    agents: [{ agent: "colonizer", projects: [{ project: "EarthOS.world", sessions: 191, latest_at: "2026-10-15T13:00:00Z" }] }],
  });
  assert.deepEqual(projects.map((p) => p.key), ["earthos-world", "livingbrain"]);
  assert.equal(projects[0].sessions, 191, "the tree's count, under the canonical key");
  assert.deepEqual(projects[0].agents, ["claude", "codex"]);
  assert.deepEqual(projects[0].sections, ["index", "learnings"]);
  const model = projectSections(projects[0].pages);
  assert.deepEqual(model.sections.map((s) => [s.page, s.entries.map((e) => e.agent)]), [
    ["index", ["codex"]],
    ["learnings", ["claude", "codex"]],
  ]);
  assert.equal(model.topics[0].title, "The auth redirect loop");
  assert.equal(model.rollup.slug, "coding-agents--projects--earthos-world");
  assert.deepEqual(agentsModel(WIKI_PAGES.pages).map((a) => a.label), ["Codex"]);
});

test("coding-agents: ?view=coding-agents shows the compiled pages as a folder tree", async () => {
  reset({ search: "?view=coding-agents" });
  seedTriggers();
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url.startsWith("/v1/pages?limit=200")) return json(CA_PAGES);
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();
  assert.equal(el("coding-agents-view").hidden, false);
  assert.equal(el("home-view").hidden, true);
  assert.equal(el("coding-agents-section").hidden, false);
  assert.equal(el("ca-empty").hidden, true);
  const folders = walk(el("coding-agents-tree"), "summary").map((n) => n.textContent);
  assert.deepEqual(folders, ["coding-agents/", "claude/", "demo/", "codex/", "demo/", "projects/"]);
  const links = walk(el("coding-agents-tree"), "a").map((a) => a.href);
  assert.ok(links.includes("wiki.html?slug=coding-agents--claude--demo--decisions"), String(links));
  assert.ok(links.includes("wiki.html?slug=coding-agents--projects--demo"));
});

test("coding-agents: the sessions view shows each session's status, its pages, and Process now", async () => {
  reset({ search: "?view=sessions" });
  seedTriggers();
  let queue = {
    counts: { queued: 0, done: 1, "skipped-no-model": 0 },
    model: { connected: false, role: "main" },
    budget: { daily_tokens: 400000, spent_today: 0 },
    jobs: [
      {
        source_id: "01J0000000000000000000000B",
        status: "done",
        pages: ["coding-agents--claude--demo--decisions", "coding-agents--log"],
      },
    ],
  };
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url === "/v1/sources/projects") return json(PROJECTS);
    if (url === "/v1/sources?kind=agent_log&limit=50") return json({ ...SESSIONS, next_cursor: null });
    if (url === "/v1/coding-agents/queue?limit=100") return json(queue);
    if (url === "/v1/coding-agents/process" && method === "POST") {
      queue = { ...queue, counts: { done: 1, failed: 1 } };
      return json({
        processed: [{ source_id: "x", status: "failed", pages: [], error: "model-http 429: busy" }],
        counts: {},
      });
    }
    if (url === "/v1/coding-agents/retry" && method === "POST") {
      queue = { ...queue, counts: { done: 1, queued: 1 } };
      return json({ requeued: 1, resumed_parked: { jobs: 2, units: 1 }, counts: queue.counts });
    }
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();
  assert.equal(el("ca-card").hidden, false);
  assert.equal(el("ca-summary").textContent, "1 compiled");
  assert.equal(el("ca-retry").hidden, true, "nothing failed, nothing to retry");
  assert.match(el("ca-model").textContent, /No model connected/);
  assert.match(el("ca-model").textContent, /Settings → Models/);
  const status = walk(el("sessions-list"), ".ca-status")[0];
  assert.equal(status.textContent, "Compiled");
  const wrote = walk(el("sessions-list"), ".ca-pages")[0];
  assert.match(wrote.textContent, /claude\/demo\/decisions/);
  assert.doesNotMatch(wrote.textContent, /log/, "the log is not a page a session wrote");

  await el("ca-process").click();
  await drain();
  const post = calls.find((call) => call.url === "/v1/coding-agents/process");
  assert.equal(post.method, "POST");
  assert.deepEqual(JSON.parse(post.init.body), { max: 1 });
  assert.equal(el("ca-result").textContent, "Failed (model-http 429: busy)");
  assert.equal(el("ca-summary").textContent, "1 compiled · 1 failed");
  assert.equal(el("ca-process").disabled, false);
  assert.equal(el("ca-retry").hidden, false);

  await el("ca-retry").click();
  await drain();
  const retry = calls.find((call) => call.url === "/v1/coding-agents/retry");
  assert.equal(retry.method, "POST");
  assert.equal(
    el("ca-result").textContent,
    "1 session requeued. 3 parked jobs waiting for a model resumed.",
  );
  assert.equal(el("ca-retry").hidden, true);
});

test("coding-agents: parked jobs alone show Retry, and it says they resumed", async () => {
  reset({ search: "?view=sessions" });
  seedTriggers();
  let queue = {
    counts: { queued: 0, done: 0, "skipped-no-model": 2 },
    model: { connected: false, role: "main" },
    budget: { daily_tokens: 400000, spent_today: 0 },
    jobs: [],
  };
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url === "/v1/sources/projects") return json(PROJECTS);
    if (url === "/v1/sources?kind=agent_log&limit=50") return json({ ...SESSIONS, next_cursor: null });
    if (url === "/v1/coding-agents/queue?limit=100") return json(queue);
    if (url === "/v1/coding-agents/retry" && method === "POST") {
      queue = { ...queue, counts: { queued: 2, "skipped-no-model": 0 } };
      return json({ requeued: 0, resumed_parked: { jobs: 2, units: 0 }, counts: queue.counts });
    }
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();
  assert.equal(el("ca-retry").hidden, false, "parked jobs can be resumed from here");

  await el("ca-retry").click();
  await drain();
  assert.equal(el("ca-result").textContent, "0 sessions requeued. 2 parked jobs waiting for a model resumed.");
  assert.equal(el("ca-retry").hidden, true);
});

test("coding-agents: a queue the API cannot answer hides the card, never the sessions", async () => {
  reset({ search: "?view=sessions" });
  seedTriggers();
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url === "/v1/sources/projects") return json(PROJECTS);
    if (url === "/v1/sources?kind=agent_log&limit=50") return json({ ...SESSIONS, next_cursor: null });
    return json({ title: "boom" }, 500);
  };
  await wikiPage(globalThis.LB.api);
  await drain();
  assert.equal(el("ca-card").hidden, true);
  assert.equal(walk(el("sessions-list"), ".wiki-row__slug").length, 1);
  assert.equal(walk(el("sessions-list"), ".ca-status").length, 0);
});

/* ------------------------------------------- temporal facts (facts.js) */

const SHOE_KEY = {
  subject: "person/ada",
  attribute: "shoe_size",
  scope: "personal",
  cardinality: "one",
  current: [
    {
      id: "fact-b",
      value: "41",
      unit: "EU",
      valid_from: "2026",
      valid_to: null,
      observed_at: "2026-10-15T09:12:00Z",
      status: "current",
    },
  ],
  history: [
    {
      id: "fact-b",
      value: "41",
      unit: "EU",
      valid_from: "2026",
      valid_to: null,
      observed_at: "2026-10-15T09:12:00Z",
      status: "current",
    },
    {
      id: "fact-a",
      value: "40",
      unit: "EU",
      valid_from: "2025",
      valid_to: "2026",
      observed_at: "2025-03-02T10:00:00Z",
      status: "superseded",
      superseded_by: "fact-b",
    },
  ],
};

const SHOE_KEY_2025 = {
  ...SHOE_KEY,
  current: [SHOE_KEY.history[1]],
};

test("facts: the pure helpers build paths and a Current + History description", async () => {
  const { factsPath, factsDescription, displayValue, displayRange } = await import(
    "../assets/facts.js"
  );
  assert.equal(factsPath(), "/v1/facts");
  assert.equal(
    factsPath({ subject: "person/ada", asOf: "2025-06-01" }),
    "/v1/facts?subject=person%2Fada&as_of=2025-06-01",
  );
  assert.equal(factsPath({ asOf: "not a date" }), "/v1/facts", "a bad date is not sent");
  assert.equal(displayValue({ value: "41", unit: "EU" }), "41 EU");
  assert.equal(displayValue({ value: "jj" }), "jj");
  assert.equal(displayRange({ valid_from: "2025", valid_to: "2026" }), "2025 – 2026");
  assert.equal(displayRange({ valid_from: "2026" }), "2026 – now");
  assert.deepEqual(factsDescription([]), []);
  assert.deepEqual(factsDescription(undefined), []);

  const description = factsDescription([SHOE_KEY]);
  const text = JSON.stringify(description);
  assert.match(text, /"Current"/);
  assert.match(text, /"History"/);
  assert.match(text, /41 EU/);
  assert.match(text, /2025 – 2026/);
  assert.match(text, /superseded/);
  // Text only: nothing in a description is markup.
  assert.ok(!text.includes("<"), "no markup in a fact description");
});

test("facts: a page with facts shows the card, and the as-of picker re-reads it", async () => {
  reset({ search: "?slug=ada" });
  seedTriggers();
  seedPageCards();
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url === "/v1/sources/projects") return json({ projects: [] });
    if (url === "/v1/pages/ada") {
      return json({
        ...HELLO,
        slug: "ada",
        title: "Ada",
        entity_type: "person",
        subject: "person/ada",
        facts: [SHOE_KEY],
      });
    }
    if (url === "/v1/facts?subject=person%2Fada&as_of=2025-12-31") {
      return json({ as_of: "2025-12-31", keys: [SHOE_KEY_2025] });
    }
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();

  assert.equal(el("page-facts-card").hidden, false);
  const cells = () => walk(el("page-facts"), "td").map((n) => n.textContent);
  assert.deepEqual(cells(), ["shoe_size", "41 EU", "2026 – now", "2026-10-15", "2026-10-15"]);
  const history = walk(el("page-facts"), "li").map((n) => n.textContent);
  assert.equal(history.length, 2);
  assert.match(history[1], /2025 – 2026.*shoe_size = 40 EU.*superseded/);

  el("page-facts-as-of").value = "2025-12-31";
  await el("page-facts-as-of").fire("change");
  await drain();
  assert.deepEqual(cells(), ["shoe_size", "40 EU", "2025 – 2026", "2025-03-02", "2025-03-02"]);
});

test("facts: a page without facts has no Facts card", async () => {
  reset({ search: "?slug=hello" });
  seedTriggers();
  seedPageCards();
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url === "/v1/pages/hello") return json(HELLO);
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();
  assert.equal(el("page-facts-card").hidden, true);
});

/** The stubs keep a class in `attrs.class` (setAttribute does not fill
 * `className`), so the Facts tests find rendered nodes by their class here. */
function withClass(root, name) {
  const out = [];
  const visit = (node) => {
    for (const child of node.children || []) {
      if (child && child.attrs && String(child.attrs.class || "").split(/\s+/).includes(name)) out.push(child);
      visit(child);
    }
  };
  visit(root);
  return out;
}

test("facts: ?view=facts lists every timeline, as of a date and back to now", async () => {
  reset({ search: "?view=facts" });
  seedTriggers();
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url === "/v1/sources/projects") return json({ projects: [] });
    if (url === "/v1/facts") return json({ as_of: "now", keys: [SHOE_KEY] });
    if (url === "/v1/facts?as_of=2025-06-01") return json({ as_of: "2025-06-01", keys: [SHOE_KEY_2025] });
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();
  assert.equal(el("facts-view").hidden, false);
  assert.equal(el("home-view").hidden, true);
  const titles = () => withClass(el("facts-list"), "fact__title").map((n) => n.textContent);
  const values = () => withClass(el("facts-list"), "fact__value").map((n) => n.textContent);
  assert.deepEqual(titles(), ["Shoe size"], "the attribute reads as a title");
  assert.equal(withClass(el("facts-list"), "fact__title")[0].getAttribute("title"), "shoe_size", "the raw key is on hover");
  assert.deepEqual(values(), ["41 EU"]);
  assert.equal(withClass(el("facts-list"), "facts-subject__name")[0].textContent, "ada");
  assert.equal(el("facts-status").textContent, "1 fact");
  assert.equal(el("facts-now").disabled, true, "Today is off when the view is already today");

  el("facts-as-of").value = "2025-06-01";
  await el("facts-as-of").fire("change");
  await drain();
  // Confirmed 2025-03-02, so 91 days stale by 2025-06-01: hidden until asked for.
  assert.deepEqual(values(), []);
  assert.equal(el("facts-status").textContent, `0 facts · as of ${formatDay("2025-06-01")} · 1 stale fact hidden`);
  el("facts-show-stale").checked = true;
  await el("facts-show-stale").fire("change");
  assert.deepEqual(values(), ["40 EU"]);
  assert.equal(el("facts-status").textContent, `1 fact · as of ${formatDay("2025-06-01")}`);
  assert.equal(el("facts-now").disabled, false);

  await el("facts-now").click();
  await drain();
  assert.equal(el("facts-as-of").value, "");
  assert.deepEqual(values(), ["41 EU"]);
});

test("facts: search filters as you type, sort and clear work, and an empty search says so", async () => {
  reset({ search: "?view=facts" });
  seedTriggers();
  const OTHER = {
    subject: "project/checkout",
    attribute: "payment_processor",
    scope: "shared",
    cardinality: "one",
    current: [{ ...SHOE_KEY.current[0], id: "fact-p", value: "Stripe Connect", unit: null, valid_from: "2026-10", observed_at: "2026-10-10T09:00:00Z", last_confirmed_at: "2026-10-10T09:00:00Z" }],
    history: [],
  };
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url === "/v1/sources/projects") return json({ projects: [] });
    if (url === "/v1/facts") return json({ as_of: "now", keys: [SHOE_KEY, OTHER] });
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();
  const titles = () => withClass(el("facts-list"), "fact__title").map((n) => n.textContent);
  const subjects = () => withClass(el("facts-rail"), "facts-rail__name").map((n) => n.textContent);
  assert.deepEqual(subjects(), ["ada", "checkout"], "the rail lists subjects with their counts; ties sort by name");
  assert.equal(walk(el("facts-jump"), "option").length, 2, "and so does the select on narrow screens");
  assert.equal(el("facts-status").textContent, "2 facts");

  el("facts-search").value = "stripe";
  await el("facts-search").fire("input");
  await new Promise((resolve) => setTimeout(resolve, 200));
  await drain();
  assert.deepEqual(titles(), ["Payment processor"]);
  assert.equal(el("facts-status").textContent, "1 fact");

  el("facts-search").value = "nothing matches this";
  await el("facts-search").fire("input");
  await new Promise((resolve) => setTimeout(resolve, 200));
  await drain();
  assert.deepEqual(titles(), []);
  assert.match(withClass(el("facts-list"), "facts-none")[0].textContent, /No facts match/u);
  await el("facts-view").fire("click", { target: walk(el("facts-list"), "[data-clear]")[0] });
  await drain();
  assert.equal(el("facts-search").value, "");
  assert.equal(titles().length, 2);

  el("facts-sort").value = "recent";
  await el("facts-sort").fire("change");
  await drain();
  // Shoe size was observed on the 15th, Payment processor on the 10th: the
  // recent sort puts the newer first, where the A to Z order had it last.
  assert.deepEqual(titles(), ["Shoe size", "Payment processor"]);
});

test("facts: the subject rail jumps to a section, and Show more expands a value in place", async () => {
  reset({ search: "?view=facts" });
  seedTriggers();
  const LONG = "word ".repeat(60).trim();
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url === "/v1/sources/projects") return json({ projects: [] });
    if (url === "/v1/facts") {
      return json({
        as_of: "now",
        keys: [
          { ...SHOE_KEY, current: [{ ...SHOE_KEY.current[0], value: LONG, observed_at: "2026-10-10T09:00:00Z", last_confirmed_at: "2026-10-10T09:00:00Z" }], history: [] },
        ],
      });
    }
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();
  const more = walk(el("facts-list"), "[data-more]")[0];
  assert.equal(more.textContent, "Show more");
  await el("facts-view").fire("click", { target: more });
  assert.equal(more.textContent, "Show less");
  assert.equal(more.getAttribute("aria-expanded"), "true");

  const before = scrolled.length;
  const jump = walk(el("facts-rail"), "[data-jump]")[0];
  await el("facts-view").fire("click", { target: jump });
  assert.equal(scrolled.length, before + 1);
  assert.equal(scrolled[scrolled.length - 1].attrs.id, "facts-subject-0", "the rail scrolls to its section");
});

test("facts: signed out, the Facts view asks to sign in and fetches nothing", async () => {
  reset({ search: "?view=facts" });
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json({ title: "unauthorized" }, 401);
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();
  assert.equal(el("facts-view").hidden, false);
  assert.ok(!calls.some((call) => call.url.startsWith("/v1/facts")));
  assert.match(el("wiki-notice").textContent, /Sign in/);
});

test("ask: a pending answer waits for the asker's own runner, then asks again", async () => {
  const calls = [];
  let asks = 0;
  const statuses = ["claimed", "done"];
  const fakeApi = async (path, options = {}) => {
    calls.push(`${options.method || "GET"} ${path}`);
    if (path === "/v1/ask") {
      asks += 1;
      return asks === 1
        ? { status: "pending", job: "mj-1", poll: "/v1/model-jobs/mj-1", retry_after: 2 }
        : { answer: "In Postgres [1].", citations: [], mode: "model" };
    }
    return { id: "mj-1", status: statuses.shift() };
  };
  let waits = 0;
  const slept = [];
  const answer = await askWithRunner(fakeApi, { question: "q" }, {
    onWait: () => (waits += 1),
    sleep: async (ms) => slept.push(ms),
  });
  assert.equal(answer.answer, "In Postgres [1].");
  assert.equal(waits, 1);
  assert.deepEqual(slept, [2000, 2000]);
  assert.deepEqual(calls, [
    "POST /v1/ask",
    "GET /v1/model-jobs/mj-1",
    "GET /v1/model-jobs/mj-1",
    "POST /v1/ask",
  ]);

  // An answer that is not pending is returned as it is, with no wait.
  const direct = await askWithRunner(async () => ({ answer: "a" }), { question: "q" }, {
    onWait: () => assert.fail("no wait"),
  });
  assert.equal(direct.answer, "a");

  // A runner that never answers gives up with a message naming it.
  const stuck = async (path) =>
    path === "/v1/ask"
      ? { status: "pending", poll: "/v1/model-jobs/mj-2", retry_after: 1 }
      : { status: "queued" };
  await assert.rejects(
    askWithRunner(stuck, { question: "q" }, { sleep: async () => {}, maxWaitMs: 3000 }),
    /did not answer in time/,
  );
  // A poll path outside the jobs routes is never followed.
  await assert.rejects(
    askWithRunner(async () => ({ status: "pending", poll: "https://evil.example/x" }), {}),
    /no job/,
  );
});

/* ------------------------------------------------------- subagents (v3) */

/** A version-3 record: the main agent on two models, one background
 * subagent that launched a nested one. */
const SUB_RECORD = {
  schema_version: 3,
  agent: "claude_code",
  collector_version: 3,
  detail: "full",
  session_id: "9a8b7c6d",
  repo: "~/code/demo",
  branch: "main",
  models: ["claude-opus-4-5", "claude-sonnet-4-5"],
  started_at: "2026-10-02T10:00:00.000Z",
  ended_at: "2026-10-02T10:03:00.000Z",
  turns: [
    { role: "user", at: "2026-10-02T10:00:00.000Z", text: "Audit the logging module", tool_calls: [] },
    {
      role: "assistant",
      at: "2026-10-02T10:00:05.000Z",
      model: "claude-opus-4-5",
      text: "Sending an explorer first.",
      tool_calls: [
        {
          name: "Agent",
          format: "text",
          input: "Find every logger in src/",
          output: "Two loggers.",
          status: "ok",
          subagent: {
            id: "a1111",
            name: "Explore",
            description: "Survey logging <b>bold?</b>",
            models: ["claude-haiku-4-5"],
            started_at: "2026-10-02T10:00:06.500Z",
            ended_at: "2026-10-02T10:01:30.000Z",
            status: "completed",
            totals: { input_tokens: 180, output_tokens: 45, cache_read_tokens: 0, cache_creation_tokens: 0, reasoning_tokens: 0, cost_usd: null },
            turns: [
              { role: "user", at: null, text: "Find every logger in src/", tool_calls: [] },
              {
                role: "assistant",
                at: null,
                model: "claude-haiku-4-5",
                text: "Looking.",
                tool_calls: [{ name: "Bash", format: "shell", input: "rg -n logger src", output: "src/a.rs:1", status: "ok" }],
              },
              {
                role: "assistant",
                at: null,
                text: "",
                tool_calls: [
                  {
                    name: "Agent",
                    input: "Check a.rs",
                    output: "a.rs is fine.",
                    status: "ok",
                    subagent: {
                      id: "a2222",
                      name: "general-purpose",
                      description: "Deep check",
                      models: ["claude-haiku-4-5"],
                      started_at: null,
                      ended_at: null,
                      status: "failed",
                      totals: {},
                      turns: [
                        { role: "user", at: null, text: "Check a.rs", tool_calls: [] },
                        { role: "assistant", at: null, model: "claude-haiku-4-5", text: "a.rs is fine.", tool_calls: [] },
                      ],
                      truncated: false,
                      omitted_turns: 0,
                    },
                  },
                ],
              },
              { role: "assistant", at: null, text: "Two loggers: **a.rs** and b.rs.", tool_calls: [] },
            ],
            truncated: true,
            omitted_turns: 7,
          },
        },
      ],
    },
    { role: "assistant", at: "2026-10-02T10:03:00.000Z", model: "claude-sonnet-4-5", text: "Both fixed.", tool_calls: [] },
  ],
  totals: { input_tokens: 300, output_tokens: 50, cache_read_tokens: 0, cache_creation_tokens: 0, reasoning_tokens: 0, cost_usd: null },
  outcome: null,
  truncated: true,
  omitted_turns: 0,
  omitted_subagent_turns: 7,
};

test("subagents: a card in the call that launched it — header, task, answer, steps on request", () => {
  assert.equal(formatDuration("2026-10-02T10:00:06.500Z", "2026-10-02T10:01:30.000Z"), "1m 23s");
  assert.equal(formatDuration("x", "y"), "");
  assert.deepEqual(subagentHeader(SUB_RECORD.turns[1].tool_calls[0].subagent), [
    "Explore",
    "claude-haiku-4-5",
    "1m 23s",
    "225 tokens",
    "completed",
  ]);
  const subs = [];
  const nodes = transcriptNodes(SUB_RECORD, subs);
  const all = textOf(nodes);
  assert.match(all, /Model: claude-opus-4-5/);
  assert.match(all, /Subagents: 2 \(Explore ×1, general-purpose ×1\)/);
  assert.match(all, /7 subagent turn\(s\) were left out/);
  assert.match(all, /🤖 Explore · claude-haiku-4-5 · 1m 23s · 225 tokens · completed/);
  // The task is text, never markup.
  assert.match(all, /Survey logging <b>bold\?<\/b>/);
  assert.equal(tagged(nodes, "b").length, 0);
  // The answer is markdown; the steps wait behind a button.
  assert.match(all, /Two loggers: a\.rs and b\.rs\./);
  assert.doesNotMatch(all, /rg -n logger/, "steps are not drawn until asked");
  const buttons = tagged(nodes, "button");
  assert.equal(buttons.length, 1);
  assert.equal(textOf(buttons[0]), "Show 3 steps");
  assert.equal(buttons[0].attrs["data-sub"], "0");
  assert.equal(subs.length, 1);
  // The model badge: only where the main agent's model changes.
  const badges = tagged(nodes, "span").filter((n) => n.attrs && n.attrs.class === "pill transcript-model");
  assert.deepEqual(badges.map(textOf), ["claude-sonnet-4-5"]);

  const steps = subagentStepNodes(subs[0], subs);
  const stepText = textOf(steps);
  assert.match(stepText, /rg -n logger src/);
  assert.match(stepText, /🤖 general-purpose · claude-haiku-4-5 · failed/);
  assert.doesNotMatch(stepText, /Two loggers: /, "the answer is on the card, not repeated");
  assert.equal(subs.length, 2, "the nested card registers its subagent");
});

test("subagents: Show steps draws a subagent's turns once, and its nested card works too", async () => {
  reset({ search: "?view=sessions&source=01J0000000000000000000000D" });
  seedTriggers();
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url === "/v1/sources/projects") return json(PROJECTS);
    if (url === "/v1/sources/01J0000000000000000000000D") {
      return json({ id: "01J0000000000000000000000D", kind: "agent_log", project: "demo", body: JSON.stringify(SUB_RECORD) });
    }
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();
  const body = el("transcript-body");
  assert.doesNotMatch(body.textContent, /rg -n logger/);
  const button = walk(body, "[data-sub]")[0];
  assert.equal(button.textContent, "Show 3 steps");
  await button.click();
  const steps = walk(body, "[data-sub-steps]")[0];
  assert.equal(steps.hidden, false);
  assert.match(steps.textContent, /rg -n logger src/);
  assert.equal(button.textContent, "Hide steps");
  assert.equal(button.getAttribute("aria-expanded"), "true");
  const nested = walk(steps, "[data-sub]")[0];
  await nested.click();
  assert.match(steps.textContent, /Check a\.rs/);
  await button.click();
  assert.equal(steps.hidden, true);
  assert.equal(button.textContent, "Show 3 steps");
});

test("transcript nodes: a version-2 record (no subagents, no models per turn) still reads", () => {
  const all = textOf(transcriptNodes({ ...RECORD, schema_version: 2 }));
  assert.match(all, /Fix all the TODOs/);
  assert.doesNotMatch(all, /Subagents:/);
  assert.doesNotMatch(all, /🤖/);
});

const CA_SESSIONS = {
  agents: [
    {
      agent: "claude_code",
      folder: "claude",
      sessions: 3,
      compiled: 1,
      projects: [{ project: "demo", sessions: 3, compiled: 1, statuses: { done: 1, queued: 2 }, latest_at: "x" }],
    },
    {
      agent: "colonizer",
      folder: "colonizer",
      sessions: 489,
      compiled: 0,
      projects: [{ project: "harness", sessions: 489, compiled: 0, statuses: { queued: 480, failed: 9 }, latest_at: "x" }],
    },
  ],
  truncated: false,
};

test("coding-agents tree: synced agents and projects are folders before any page is compiled", () => {
  assert.equal(projectSegment("My Repo.v2"), "my-repo-v2");
  assert.equal(projectSegment(""), "no-project");
  const tree = codingAgentsTree(CA_PAGES.pages, CA_SESSIONS);
  assert.deepEqual(tree.agents.map((a) => a.agent), ["claude", "codex", "colonizer"]);
  const colonizer = tree.agents[2];
  assert.equal(colonizer.info.agent, "colonizer");
  assert.equal(colonizer.info.sessions, 489);
  assert.deepEqual(colonizer.projects.map((p) => p.project), ["harness"]);
  assert.equal(colonizer.projects[0].pages.length, 0);
  assert.equal(folderSummary(colonizer.projects[0].info), "489 sessions · 0 compiled · 480 queued · 9 failed");
  // A compiled folder and its synced sessions are one folder.
  const claudeDemo = tree.agents[0].projects[0];
  assert.equal(claudeDemo.pages.length, 2);
  assert.equal(claudeDemo.info.sessions, 3);
  assert.equal(sessionsHref("colonizer", "harness"), "wiki.html?view=sessions&agent=colonizer&project=harness");
  assert.equal(sessionsPath({ agent: "colonizer" }), "/v1/sources?kind=agent_log&limit=50&agent=colonizer");
  assert.equal(sessionsPath({ agent: "Bad One" }), "/v1/sources?kind=agent_log&limit=50");
});

test("coding-agents: the view shows colonizer/ from synced sessions, linking to its sessions", async () => {
  reset({ search: "?view=coding-agents" });
  seedTriggers();
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url.startsWith("/v1/pages?limit=200")) return json({ pages: [] });
    if (url === "/v1/coding-agents/tree") return json(CA_SESSIONS);
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();
  assert.equal(el("coding-agents-section").hidden, false);
  const folders = walk(el("coding-agents-tree"), "summary").map((n) => n.textContent);
  assert.deepEqual(folders, [
    "coding-agents/",
    "claude/ 3 sessions · 1 compiled",
    "demo/ 3 sessions · 1 compiled",
    "colonizer/ 489 sessions · 0 compiled",
    "harness/ 489 sessions · 0 compiled",
  ]);
  const links = walk(el("coding-agents-tree"), "a").map((a) => a.href);
  assert.ok(links.includes("wiki.html?view=sessions&agent=colonizer"), String(links));
  assert.ok(links.includes("wiki.html?view=sessions&agent=colonizer&project=harness"), String(links));
  const info = walk(el("coding-agents-tree"), "li").find((li) => /Open all 489 sessions/.test(li.textContent));
  assert.match(info.textContent, /480 queued · 9 failed/);
});

test("agent sessions: ?agent= filters the list, and the agent select changes it", async () => {
  reset({ search: "?view=sessions&agent=colonizer" });
  seedTriggers();
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url === "/v1/sources/projects") return json(PROJECTS);
    if (url.startsWith("/v1/sources?kind=agent_log")) return json({ sources: [], next_cursor: null });
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();
  assert.ok(calls.some((call) => call.url === "/v1/sources?kind=agent_log&limit=50&agent=colonizer"));
  const select = el("sessions-agent");
  assert.equal(select.hidden, false);
  assert.equal(select.value, "colonizer");
  assert.deepEqual(
    select.children.map((o) => o.value),
    ["", "claude_code", "codex", "colonizer"],
  );
  select.value = "codex";
  await select.fire("change");
  await drain();
  assert.ok(calls.some((call) => call.url === "/v1/sources?kind=agent_log&limit=50&agent=codex"));
  assert.equal(replacedUrls.at(-1), "wiki.html?view=sessions&agent=codex");
});

/* ------------------------------------- the shell's views (ux-navigation) */

test("ask view: one box — Enter with no page picked asks, and the answer lands under it", async () => {
  reset({ search: "?view=ask" });
  seedTriggers();
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url.startsWith("/v1/search?")) {
      return json({ results: [{ title: "Kestrel", url: "https://app.test/wiki?slug=kestrel&scope=shared", snippet: "k" }] });
    }
    if (method === "POST" && url === "/v1/ask") {
      return json({
        answer: "Kestrel is owned by [[ada]].",
        citations: [{ title: "Kestrel", url: "https://app.test/wiki?slug=kestrel", quote: "owned by ada" }],
      });
    }
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();
  assert.equal(el("ask-view").hidden, false);
  assert.equal(el("home-view").hidden, true);
  assert.equal(globalThis.document.body.dataset.view, "ask");
  assert.ok(focused.includes(el("wiki-search")), "the box takes the focus");
  assert.equal(el("ask-view-result").hidden, true, "nothing asked yet");

  el("wiki-search").value = "who owns kestrel";
  await el("wiki-search").fire("input");
  await wait(260);
  await drain();
  assert.equal(el("wiki-search-results").hidden, false, "pages match as you type");

  // Enter with no option active: a question, not the first hit.
  await el("wiki-search").fire("keydown", { key: "Enter" });
  await drain();
  assert.deepEqual(assigns, [], "no page was opened");
  const post = calls.find((call) => call.method === "POST");
  assert.equal(post.url, "/v1/ask");
  assert.deepEqual(JSON.parse(post.init.body), { question: "who owns kestrel" });
  assert.equal(el("ask-view-result").hidden, false);
  assert.match(el("ask-view-answer").textContent, /Kestrel is owned by/);
  assert.equal(walk(el("ask-view-citations"), "blockquote").length, 1);
  assert.equal(el("wiki-search-results").hidden, true, "the hits make way for the answer");
  assert.equal(replacedUrls.at(-1), "wiki.html?view=ask&q=who%20owns%20kestrel", "the address keeps the question");
  assert.equal(el("ask-go").disabled, false);

  // The form's submit (the Ask button) is the same path.
  calls.length = 0;
  await el("wiki-search-form").fire("submit");
  await drain();
  assert.ok(calls.some((call) => call.method === "POST" && call.url === "/v1/ask"));
});

test("ask view: ?q= pre-fills the box and asks at once; signed out says sign in", async () => {
  reset({ search: "?view=ask&q=what%20is%20kestrel" });
  seedTriggers();
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (method === "POST" && url === "/v1/ask") return json({ answer: "A router.", citations: [] });
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();
  assert.equal(el("wiki-search").value, "what is kestrel");
  assert.equal(JSON.parse(calls.find((call) => call.method === "POST").init.body).question, "what is kestrel");
  assert.match(el("ask-view-answer").textContent, /A router\./);

  reset({ search: "?view=ask&q=x" });
  seedTriggers();
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json({ title: "Unauthorized" }, 401);
    return json({ title: "Unauthorized" }, 401);
  };
  await wikiPage(globalThis.LB.api);
  await drain();
  assert.equal(calls.some((call) => call.method === "POST"), false, "nothing is asked signed out");
  assert.match(el("wiki-notice").textContent, /Sign in to search and ask the brain\./);
});

test("imports view: lists kind=import sources and reads one when it is opened", async () => {
  reset({ search: "?view=imports" });
  seedTriggers();
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url === "/v1/sources?kind=import&limit=50") {
      return json({
        sources: [
          { id: "01HIMPORTA", kind: "import", scope: "shared", path: "notes/one.md", project: "demo", created_at: "2026-10-01T10:00:00Z" },
          { id: "01HIMPORTB", kind: "import", scope: "personal", path: "notes/two.md", project: null, created_at: "2026-10-02T10:00:00Z" },
        ],
        next_cursor: null,
      });
    }
    if (url === "/v1/sources/01HIMPORTA") return json({ id: "01HIMPORTA", kind: "import", body: "---\ntitle: One\n---\n# One\n\nA **note**." });
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();
  assert.equal(el("imports-view").hidden, false);
  assert.equal(el("imports-list").hidden, false);
  assert.equal(el("imports-empty").hidden, true);
  assert.equal(el("imports-more").hidden, true);
  assert.match(el("imports-status").textContent, /2 imports/);
  const rows = el("imports-list").children;
  assert.equal(rows.length, 2);
  assert.match(rows[0].textContent, /notes\/one\.md/);
  const details = rows[0].children[0];
  details.open = true;
  await details.fire("toggle");
  await drain();
  assert.ok(calls.some((call) => call.url === "/v1/sources/01HIMPORTA"));
  assert.ok(walk(details, "strong").length > 0, "the body went through the Markdown renderer");
  assert.doesNotMatch(details.textContent, /title: One/, "the frontmatter fence is not shown");
  // Opening it again does not fetch again.
  const before = calls.length;
  await details.fire("toggle");
  assert.equal(calls.length, before);
});

test("imports view: none yet explains how to import", async () => {
  reset({ search: "?view=imports" });
  seedTriggers();
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url.startsWith("/v1/sources?kind=import")) return json({ sources: [], next_cursor: null });
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();
  assert.equal(el("imports-list").hidden, true);
  assert.equal(el("imports-empty").hidden, false);
});

test("?view=new opens the new-page form; a page shows its breadcrumbs", async () => {
  reset({ search: "?view=new" });
  seedTriggers();
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url.startsWith("/v1/pages?limit=200")) return json(PAGES);
    if (url === "/v1/pages/hello") return json(HELLO);
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();
  assert.equal(el("new-page-view").hidden, false);
  assert.ok(focused.includes(el("np-slug")));

  reset({ search: "?slug=hello" });
  seedTriggers();
  await wikiPage(globalThis.LB.api);
  await drain();
  const crumbs = walk(el("page-crumbs"), "li").map((li) => li.textContent);
  assert.deepEqual(crumbs, ["Pages", "Hello"]);
  assert.equal(walk(el("page-crumbs"), "a")[0].getAttribute("href"), "wiki.html");
  assert.equal(globalThis.document.body.dataset.view, "page");
});

/* ------------------------------------------------------- GitHub (Sources) */

const GH_COMMIT = {
  id: "01HGHCOMMIT",
  kind: "github_commit",
  scope: "shared",
  path: "github/World-360/EarthOS.world/commit/9f2c1e0000000000000000000000000000000000",
  project: "EarthOS.world",
  created_at: "2026-10-03T09:00:00Z",
};
const GH_ISSUE = {
  id: "01HGHISSUE",
  kind: "github_issue",
  scope: "shared",
  path: "github/World-360/EarthOS.world/issue/123",
  project: "EarthOS.world",
  created_at: "2026-10-01T09:00:00Z",
};
const GH_PR = {
  id: "01HGHPR",
  kind: "github_pr",
  scope: "shared",
  path: "github/World-360/EarthOS.world/pr/45",
  project: "EarthOS.world",
  created_at: "2026-10-02T09:00:00Z",
};

test("github helpers: the listing path, the permalink from a rel_path, and a citation's label", () => {
  assert.equal(validGithubKind(""), true);
  assert.equal(validGithubKind("github_pr"), true);
  assert.equal(validGithubKind("import"), false);
  assert.equal(validGithubKind("github_release"), false);

  assert.equal(githubPath({ kind: "github_issue" }), "/v1/sources?kind=github_issue&limit=50");
  assert.equal(
    githubPath({ kind: "github_pr", project: "EarthOS.world", cursor: "01JX" }),
    "/v1/sources?kind=github_pr&limit=50&project=EarthOS.world&cursor=01JX",
  );
  assert.equal(githubPath({ kind: "github_pr", project: "../x" }), "/v1/sources?kind=github_pr&limit=50");

  assert.deepEqual(githubRef(GH_ISSUE.path), {
    label: "#123",
    repo: "World-360/EarthOS.world",
    url: "https://github.com/World-360/EarthOS.world/issues/123",
  });
  assert.equal(githubRef(GH_PR.path).url, "https://github.com/World-360/EarthOS.world/pull/45");
  assert.equal(githubRef(GH_COMMIT.path).label, "9f2c1e0");
  assert.equal(githubRef(GH_COMMIT.path).url, `https://github.com/World-360/EarthOS.world/commit/${GH_COMMIT.path.split("/").pop()}`);
  // Anything that is not a GitHub path gets no link, so nothing else is linked.
  assert.equal(githubRef("notes/one.md"), null);
  assert.equal(githubRef("github/o/r/issue/abc"), null);

  assert.deepEqual(githubCitation("https://github.com/World-360/EarthOS.world/issues/123#issuecomment-9"), {
    repo: "World-360/EarthOS.world",
    label: "#123",
    url: "https://github.com/World-360/EarthOS.world/issues/123",
  });
  assert.equal(githubCitation("https://example.com/World-360/EarthOS.world/issues/123"), null);
  assert.equal(githubCitation("javascript:alert(1)"), null);
});

test("GitHub view: all kinds merge newest first, each row links to github.com", async () => {
  reset({ search: "?view=github" });
  seedTriggers();
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url === "/v1/sources/projects") return json({ projects: [{ project: "EarthOS.world", sources: 3 }] });
    if (url === "/v1/sources?kind=github_commit&limit=50") return json({ sources: [GH_COMMIT], next_cursor: null });
    if (url === "/v1/sources?kind=github_issue&limit=50") return json({ sources: [GH_ISSUE], next_cursor: null });
    if (url === "/v1/sources?kind=github_pr&limit=50") return json({ sources: [GH_PR], next_cursor: null });
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();
  assert.equal(el("github-view").hidden, false);
  assert.equal(globalThis.document.body.dataset.view, "github");
  const rows = el("github-list").children;
  assert.equal(rows.length, 3);
  // Newest first: the commit (10-03), the pull request (10-02), the issue (10-01).
  assert.match(rows[0].textContent, /World-360\/EarthOS\.world 9f2c1e0/);
  assert.match(rows[1].textContent, /#45/);
  assert.match(rows[2].textContent, /#123/);
  const links = walk(el("github-list"), "a").map((a) => a.href);
  assert.deepEqual(links, [
    `https://github.com/World-360/EarthOS.world/commit/${GH_COMMIT.path.split("/").pop()}`,
    "https://github.com/World-360/EarthOS.world/pull/45",
    "https://github.com/World-360/EarthOS.world/issues/123",
  ]);
  assert.equal(el("github-more").hidden, true, "every kind is one page, so nothing pages");
  assert.match(el("github-status").textContent, /3 items/);
  // The project filter is filled from the same list the other filters use.
  assert.equal(el("github-project").hidden, false);
});

test("GitHub view: one kind is one request, and it pages with its own cursor", async () => {
  reset({ search: "?view=github&kind=github_issue" });
  seedTriggers();
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url === "/v1/sources/projects") return json({ projects: [] });
    if (url === "/v1/sources?kind=github_issue&limit=50") {
      return json({ sources: [GH_ISSUE], next_cursor: "01HGHISSUE" });
    }
    if (url === "/v1/sources?kind=github_issue&limit=50&cursor=01HGHISSUE") {
      return json({ sources: [{ ...GH_ISSUE, id: "01HGHISSUE2", path: "github/World-360/EarthOS.world/issue/77" }], next_cursor: null });
    }
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();
  assert.equal(el("github-kind").value, "github_issue");
  assert.equal(el("github-list").children.length, 1);
  assert.equal(el("github-more").hidden, false);
  assert.equal(el("github-project").hidden, true, "no projects, no project filter");
  assert.ok(!calls.some((call) => call.url.includes("kind=github_pr")), "only the chosen kind is asked for");
  await el("github-more").fire("click");
  await drain();
  assert.equal(el("github-list").children.length, 2);
  assert.match(el("github-list").children[1].textContent, /#77/);
  assert.equal(el("github-more").hidden, true);
});

test("GitHub view: none yet explains how to connect, and signed out says sign in", async () => {
  reset({ search: "?view=github" });
  seedTriggers();
  respond = (method, url) => {
    if (url === "/v1/workspaces/me") return json(ME);
    if (url === "/v1/sources/projects") return json({ projects: [] });
    if (url.startsWith("/v1/sources?kind=github_")) return json({ sources: [], next_cursor: null });
    return json({ title: "not found" }, 404);
  };
  await wikiPage(globalThis.LB.api);
  await drain();
  assert.equal(el("github-list").hidden, true);
  assert.equal(el("github-empty").hidden, false);

  reset({ search: "?view=github" });
  seedTriggers();
  respond = (method, url) => (url === "/v1/workspaces/me" ? json({ title: "signed out" }, 401) : json({ title: "not found" }, 404));
  await wikiPage(globalThis.LB.api);
  await drain();
  assert.equal(calls.some((call) => call.url.startsWith("/v1/sources?")), false, "no listing when signed out");
  assert.match(el("wiki-notice").textContent, /Sign in to see your GitHub items/);
});
