// The app shell (`assets/shell.js`), mounted on a small stub DOM: the
// sidebar built from `nav.js` (links, reserved slots, the current item), the
// drawer and collapse toggles, the switcher, and the ⌘K palette driven by
// keyboard — open, filter, search, move, choose — asserting on the DOM and
// on where the page was sent, never on internals.

import test from "node:test";
import assert from "node:assert/strict";

/* ------------------------------------------------------------- the stubs */

function node(tag) {
  const el = {
    tagName: tag.toUpperCase(),
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
    parentNode: null,
    setAttribute(name, value) {
      this.attrs[name] = String(value);
      if (name === "id") byId.set(String(value), this);
    },
    getAttribute(name) {
      return name in this.attrs ? this.attrs[name] : null;
    },
    removeAttribute(name) {
      delete this.attrs[name];
      if (name === "open") this.open = false;
    },
    hasAttribute(name) {
      return name in this.attrs;
    },
    appendChild(child) {
      this.children.push(child);
      child.parentNode = this;
      return child;
    },
    append(...kids) {
      for (const kid of kids) this.appendChild(kid);
    },
    replaceChildren(...kids) {
      this.children = [];
      this._text = "";
      for (const kid of kids) this.appendChild(kid);
    },
    get textContent() {
      return this._text + this.children.map((c) => c.textContent).join("");
    },
    set textContent(value) {
      this._text = String(value);
      this.children = [];
    },
    addEventListener(type, fn) {
      (this.listeners[type] = this.listeners[type] || []).push(fn);
    },
    async fire(type, event = {}) {
      const e = { target: this, preventDefault() {}, ...event };
      for (const fn of this.listeners[type] || []) await fn(e);
    },
    async click() {
      await this.fire("click");
    },
    focus() {
      doc.activeElement = this;
    },
    scrollIntoView() {},
    showModal() {
      this.open = true;
    },
    close() {
      this.open = false;
    },
    contains(other) {
      for (let at = other; at; at = at.parentNode) if (at === this) return true;
      return false;
    },
    closest(sel) {
      for (let at = this; at; at = at.parentNode) if (at.tagName && matches(at, sel)) return at;
      return null;
    },
    querySelector(sel) {
      return all(this, sel)[0] || null;
    },
    querySelectorAll(sel) {
      return all(this, sel);
    },
  };
  return el;
}

function matches(el, sel) {
  return sel.split(",").some((one) => {
    const s = one.trim();
    if (s.startsWith("#")) return el.attrs.id === s.slice(1);
    if (s.startsWith(".")) return String(el.className).split(/\s+/).includes(s.slice(1));
    if (s.startsWith("[")) {
      const [name, value] = s.slice(1, -1).split("=");
      if (!(name in el.attrs)) return false;
      return value === undefined || el.attrs[name] === value.replace(/"/g, "");
    }
    return el.tagName === s.toUpperCase();
  });
}

function all(root, sel) {
  const out = [];
  const visit = (parent) => {
    for (const child of parent.children || []) {
      if (child.tagName && matches(child, sel)) out.push(child);
      visit(child);
    }
  };
  visit(root);
  return out;
}

const byId = new Map();
let doc;
const assigns = [];

function world({ pathname = "/wiki.html", search = "", hash = "" } = {}) {
  byId.clear();
  assigns.length = 0;
  const body = node("body");
  body.dataset.page = "wiki";
  const listeners = {};
  doc = {
    body,
    activeElement: null,
    createElement: (tag) => node(tag),
    createElementNS: (_ns, tag) => node(tag),
    getElementById: (id) => byId.get(id) || null,
    querySelector: (sel) => all(body, sel)[0] || null,
    querySelectorAll: (sel) => all(body, sel),
    addEventListener(type, fn) {
      (listeners[type] = listeners[type] || []).push(fn);
    },
    async key(event) {
      for (const fn of listeners.keydown || []) await fn({ target: body, preventDefault() {}, ...event });
    },
  };
  globalThis.document = doc;
  globalThis.location = { pathname, search, hash, assign: (url) => assigns.push(url) };
  globalThis.localStorage = { getItem: () => null, setItem() {}, removeItem() {} };
  // The static markup the shell expects, by id.
  for (const [id, tag] of [
    ["app-sidebar", "aside"],
    ["app-nav", "nav"],
    ["nav-collapse", "button"],
    ["nav-toggle", "button"],
    ["nav-scrim", "div"],
    ["ws-switch", "div"],
    ["ws-button", "button"],
    ["ws-menu", "div"],
    ["ws-name", "span"],
    ["user-button", "button"],
    ["user-menu", "div"],
    ["user-initial", "span"],
    ["palette-open", "button"],
  ]) {
    const el = node(tag);
    el.setAttribute("id", id);
    body.appendChild(el);
  }
  byId.get("app-sidebar").appendChild(byId.get("app-nav"));
  byId.get("ws-menu").hidden = true;
  byId.get("user-menu").hidden = true;
  byId.get("nav-scrim").hidden = true;
  return doc;
}

const { mountShell } = await import("../assets/shell.js");
const { navItems } = await import("../assets/nav.js");

const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function fakeApi(answers = {}) {
  const calls = [];
  const api = async (path, options = {}) => {
    calls.push({ path, method: options.method || "GET", body: options.body });
    for (const [prefix, answer] of Object.entries(answers)) {
      if (path.startsWith(prefix)) return typeof answer === "function" ? answer(path) : answer;
    }
    throw new Error(`unexpected ${path}`);
  };
  return { api, calls };
}

const SIGNED_IN = {
  kind: "signed-in",
  me: { workspace: { id: "T0", name: "Acme" }, member: { user_id: "u1", name: "ada" }, is_owner: true },
};

/* ------------------------------------------------------------- the tests */

test("the sidebar: a link per destination, a Soon row per slot, the current item marked", () => {
  world({ pathname: "/wiki.html", search: "?view=facts" });
  const { api } = fakeApi();
  mountShell({ api, toggleTheme() {}, signOut() {} });
  const links = all(byId.get("app-nav"), "a");
  assert.deepEqual(
    links.map((a) => a.getAttribute("href")),
    navItems().filter((item) => item.href).map((item) => item.href),
  );
  const current = links.filter((a) => a.getAttribute("aria-current") === "page");
  assert.deepEqual(current.map((a) => a.getAttribute("data-nav")), ["facts"]);
  const soon = all(byId.get("app-nav"), "[aria-disabled]");
  // Every slot is a non-link "Soon" row (none are left since ADR 0004
  // shipped agent accounts and access, but the rule still holds).
  assert.deepEqual(
    soon.map((row) => row.getAttribute("data-nav")),
    navItems().filter((item) => item.soon).map((item) => item.id),
  );
  for (const row of soon) {
    assert.equal(row.tagName, "SPAN", "a slot is not a link");
    assert.match(row.textContent, /Soon/);
  }
  // Groups are labelled lists.
  const lists = all(byId.get("app-nav"), "ul");
  assert.deepEqual(lists.map((ul) => ul.getAttribute("aria-labelledby")), [
    "nav-group-brain",
    "nav-group-sources",
    "nav-group-workspace",
    "nav-group-settings",
  ]);
});

test("setActive follows the view the page switched to", () => {
  world({ pathname: "/wiki.html", search: "" });
  const { api } = fakeApi();
  const shell = mountShell({ api, toggleTheme() {}, signOut() {} });
  assert.equal(shell.active, "pages");
  shell.setActive("ask");
  assert.equal(shell.active, "ask");
  const current = all(byId.get("app-nav"), "[aria-current]");
  assert.deepEqual(current.map((a) => a.getAttribute("data-nav")), ["ask"]);
});

test("the drawer opens with aria-expanded and closes on Escape, the scrim, or a link", async () => {
  world();
  const { api } = fakeApi();
  mountShell({ api, toggleTheme() {}, signOut() {} });
  const toggle = byId.get("nav-toggle");
  await toggle.click();
  assert.equal(doc.body.dataset.nav, "open");
  assert.equal(toggle.getAttribute("aria-expanded"), "true");
  assert.equal(byId.get("nav-scrim").hidden, false);
  assert.equal(doc.activeElement.getAttribute("data-nav"), "home", "focus moves into the drawer");
  await doc.key({ key: "Escape" });
  assert.equal(doc.body.dataset.nav, undefined);
  assert.equal(toggle.getAttribute("aria-expanded"), "false");
  assert.equal(doc.activeElement, toggle, "focus returns to the button");
  await toggle.click();
  await byId.get("nav-scrim").click();
  assert.equal(doc.body.dataset.nav, undefined);
  await toggle.click();
  const link = all(byId.get("app-nav"), "a")[2];
  await byId.get("app-sidebar").fire("click", { target: link });
  assert.equal(doc.body.dataset.nav, undefined, "following a link closes it");
});

test("collapse toggles the rail and says so", async () => {
  world();
  const { api } = fakeApi();
  mountShell({ api, toggleTheme() {}, signOut() {} });
  const button = byId.get("nav-collapse");
  assert.equal(button.getAttribute("aria-pressed"), "false");
  await button.click();
  assert.equal(doc.body.dataset.navCollapsed, "true");
  assert.equal(button.getAttribute("aria-pressed"), "true");
  assert.equal(button.getAttribute("aria-label"), "Expand the sidebar");
  await button.click();
  assert.equal(doc.body.dataset.navCollapsed, undefined);
});

test("signed in: the switcher names the workspace and lists the others to switch to", async () => {
  world();
  const { api, calls } = fakeApi({
    "/v1/workspaces/switch": {},
    "/v1/workspaces": [
      { id: "T0", name: "Acme", role: "owner", current: true },
      { id: "T1", name: "Side", role: "member", current: false },
    ],
  });
  const shell = mountShell({ api, toggleTheme() {}, signOut() {} });
  assert.equal(byId.get("ws-switch").hidden, false, "shown until told otherwise is fine; now apply");
  shell.applySession({ kind: "signed-out" });
  assert.equal(byId.get("ws-switch").hidden, true);
  assert.equal(doc.body.dataset.session, "signed-out");
  shell.applySession(SIGNED_IN);
  assert.equal(doc.body.dataset.session, "signed-in");
  assert.equal(byId.get("ws-switch").hidden, false);
  assert.equal(byId.get("ws-name").textContent, "Acme");
  assert.equal(byId.get("user-initial").textContent, "A");

  await byId.get("ws-button").click();
  assert.equal(byId.get("ws-menu").hidden, false);
  assert.equal(byId.get("ws-button").getAttribute("aria-expanded"), "true");
  const items = all(byId.get("ws-menu"), "button");
  assert.deepEqual(items.map((b) => b.disabled), [true, false], "the current one is not a switch");
  assert.equal(items[0].getAttribute("aria-current"), "true");
  await items[1].click();
  assert.deepEqual(calls.at(-1), { path: "/v1/workspaces/switch", method: "POST", body: { workspace_id: "T1" } });
  assert.deepEqual(assigns, ["/"]);
  const manage = all(byId.get("ws-menu"), "a")[0];
  assert.equal(manage.getAttribute("href"), "settings.html#members");
});

test("avatars: the workspace's and yours in the top bar, initials when there is none", async () => {
  world();
  const avatar = (id) => `/v1/avatars/av_01HZZZ${id.repeat(20)}`;
  const { api } = fakeApi({
    "/v1/workspaces": [
      { id: "T0", name: "Acme", role: "owner", current: true, avatar_url: avatar("A") },
      { id: "T1", name: "Side Project", role: "member", current: false, avatar_url: null },
    ],
  });
  const shell = mountShell({ api, toggleTheme() {}, signOut() {} });
  shell.applySession({
    kind: "signed-in",
    me: {
      workspace: { id: "T0", name: "Acme", avatar_url: avatar("A") },
      member: { user_id: "u1", name: "Ada Lovelace", avatar_url: avatar("B") },
      is_owner: true,
    },
  });
  // The switcher button gained one picture, kept beside the name.
  const pics = all(byId.get("ws-button"), ".avatar");
  assert.equal(pics.length, 1);
  assert.equal(all(pics[0], "img")[0].getAttribute("src"), avatar("A"));
  // The user button's slot shows the uploaded picture, not the initial.
  const mine = all(byId.get("user-initial"), "img");
  assert.equal(mine.length, 1);
  assert.equal(mine[0].getAttribute("src"), avatar("B"));
  assert.equal(mine[0].getAttribute("alt"), "");

  // Painting again (a name or picture changed) reuses the slot.
  shell.applySession({
    kind: "signed-in",
    me: { workspace: { id: "T0", name: "Acme" }, member: { user_id: "u1", name: "Ada Lovelace" }, is_owner: true },
  });
  assert.equal(all(byId.get("ws-button"), ".avatar").length, 1);
  assert.equal(all(byId.get("ws-button"), "img").length, 0);
  assert.equal(byId.get("user-initial").textContent, "AL");

  // The switcher's rows carry each workspace's picture or initials.
  await byId.get("ws-button").click();
  const rows = all(byId.get("ws-menu"), "button");
  assert.equal(all(rows[0], "img")[0].getAttribute("src"), avatar("A"));
  assert.equal(all(rows[1], ".avatar")[0].textContent, "SP");
  assert.ok(rows[1].textContent.includes("Side Project"));
  const links = all(byId.get("ws-menu"), "a").map((a) => a.getAttribute("href"));
  assert.deepEqual(links, ["settings.html#members", "settings.html#workspace"]);
});

test("the palette: ⌘K opens it, typing narrows, arrows move, Enter goes", async () => {
  world();
  const { api } = fakeApi();
  const shell = mountShell({ api, toggleTheme() {}, signOut() {} });
  shell.applySession(SIGNED_IN);
  const dialog = byId.get("palette");
  const input = byId.get("palette-input");
  assert.equal(dialog.open, false);
  await doc.key({ key: "k", metaKey: true });
  assert.equal(dialog.open, true);
  assert.equal(doc.activeElement, input);
  assert.equal(input.getAttribute("aria-expanded"), "true");

  input.value = "fac";
  await input.fire("input");
  const options = () => all(byId.get("palette-list"), "[role=option]");
  assert.equal(options()[0].textContent.startsWith("Facts"), true, options()[0].textContent);
  assert.equal(input.getAttribute("aria-activedescendant"), "palette-opt-0");
  await input.fire("keydown", { key: "Enter" });
  assert.deepEqual(assigns, ["wiki.html?view=facts"]);
  assert.equal(dialog.open, false, "choosing closes it");
});

test("the palette searches pages after a pause and offers to ask", async () => {
  world();
  const { api, calls } = fakeApi({
    "/v1/search": {
      results: [{ title: "Kestrel routes", url: "https://app.test/wiki?slug=kestrel-routes&scope=shared", snippet: "edge" }],
    },
  });
  const shell = mountShell({ api, toggleTheme() {}, signOut() {} });
  shell.applySession(SIGNED_IN);
  await byId.get("palette-open").click();
  const input = byId.get("palette-input");
  input.value = "kestrel";
  await input.fire("input");
  await wait(220);
  assert.ok(calls.some((call) => call.path === "/v1/search?q=kestrel&limit=6"), JSON.stringify(calls));
  const labels = all(byId.get("palette-list"), "[role=option]").map((o) => o.children[0].textContent);
  assert.deepEqual(labels, ["Kestrel routes", "Ask the brain: “kestrel”"]);
  const groups = all(byId.get("palette-list"), ".palette__group").map((g) => g.textContent);
  assert.deepEqual(groups, ["Pages", "Ask"]);
  await input.fire("keydown", { key: "ArrowDown" });
  assert.equal(input.getAttribute("aria-activedescendant"), "palette-opt-1");
  await input.fire("keydown", { key: "Enter" });
  assert.deepEqual(assigns, ["wiki.html?view=ask&q=kestrel"]);
});

test("the palette: '/' opens it outside fields but not on the Ask view; Escape closes", async () => {
  world();
  const { api } = fakeApi();
  mountShell({ api, toggleTheme() {}, signOut() {} });
  const dialog = byId.get("palette");
  await doc.key({ key: "/", target: { tagName: "INPUT" } });
  assert.equal(dialog.open, false, "a field keeps its slash");
  doc.body.dataset.view = "ask";
  await doc.key({ key: "/" });
  assert.equal(dialog.open, false, "the Ask view's own box takes it");
  delete doc.body.dataset.view;
  await doc.key({ key: "/" });
  assert.equal(dialog.open, true);
  await byId.get("palette-input").fire("keydown", { key: "Escape" });
  assert.equal(dialog.open, false);
});

test("the palette's actions call back into the page", async () => {
  world();
  let themes = 0;
  let signouts = 0;
  const { api } = fakeApi();
  const shell = mountShell({ api, toggleTheme: () => themes++, signOut: () => signouts++ });
  shell.applySession(SIGNED_IN);
  for (const [query, check] of [
    ["switch theme", () => assert.equal(themes, 1)],
    ["sign out", () => assert.equal(signouts, 1)],
  ]) {
    shell.openPalette();
    const input = byId.get("palette-input");
    input.value = query;
    await input.fire("input");
    await input.fire("keydown", { key: "Enter" });
    check();
  }
});
