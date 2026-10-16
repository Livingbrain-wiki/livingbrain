// The information architecture (`assets/nav.js`, docs/ux-navigation.md):
// every address the app ever published still lands on a sidebar
// destination, the settings anchors map onto their tabs, the breadcrumbs
// file a page where the sidebar does, and the palette offers what it should.
// Plus the static contract of the three pages: one shell, the same ids.

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { plan } from "../../tools/stamp-assets.mjs";

import {
  NAV,
  ICONS,
  navItems,
  navItem,
  activeId,
  settingsTab,
  SETTINGS_TABS,
  pageFile,
  pageCrumbs,
  transcriptCrumbs,
  matchDestinations,
  paletteOptions,
  paletteSearchPath,
  hitSlug,
  askHref,
} from "../assets/nav.js";

const at = (url) => {
  const parsed = new URL(url, "https://app.livingbrain.wiki/");
  return activeId({ pathname: parsed.pathname, search: parsed.search, hash: parsed.hash });
};

test("every address published before the shell still resolves to a destination", () => {
  const legacy = {
    "/": "home",
    "/index.html": "home",
    "/wiki.html": "pages",
    "/wiki.html?slug=kestrel": "pages",
    // The API's own citation URL, served by Workers' clean URLs.
    "/wiki?slug=q3-plan&scope=user-01HX": "pages",
    "/wiki.html?slug=kestrel&mode=edit": "pages",
    // The compiled wiki is Pages now, filed by project; a session log is
    // a source, under Agent sessions.
    "/wiki.html?slug=coding-agents--claude--demo--decisions": "pages",
    "/wiki.html?slug=agent-log-colonizer-a15fba01": "sessions",
    "/wiki.html?view=project&project=earthos-world": "pages",
    "/wiki.html?view=project&project=earthos-world&section=learnings": "pages",
    "/wiki.html?view=coding-agents": "coding-agents",
    "/wiki.html?view=sessions": "sessions",
    "/wiki.html?view=sessions&project=side": "sessions",
    "/wiki.html?view=sessions&agent=colonizer&project=x": "sessions",
    "/wiki.html?view=sessions&source=01HZZZ": "sessions",
    "/wiki.html?view=facts": "facts",
    "/settings.html": "models",
    "/settings.html#models": "models",
    "/settings.html#tokens": "tokens",
    "/settings.html#members": "members",
    "/settings.html#limits": "limits",
    "/settings.html#connect": "integrations",
    "/settings.html#agents": "agents",
    "/settings.html#access": "access",
    "/settings.html#audit": "access",
    "/brain.html": "map",
    "/settings.html#github": "integrations",
    "/wiki.html?view=github": "github",
  };
  for (const [url, id] of Object.entries(legacy)) {
    assert.equal(at(url), id, url);
    assert.ok(navItem(id), `${url} lands on a real destination`);
  }
});

test("every sidebar link is an address that resolves back to its own item", () => {
  for (const item of navItems()) {
    if (!item.href) continue;
    assert.equal(at(`/${item.href}`), item.id, item.href);
  }
});

test("the groups are Brain, Sources, Workspace, Settings, and reserved slots are not links", () => {
  assert.deepEqual(NAV.map((group) => group.label), ["Brain", "Sources", "Workspace", "Settings"]);
  const ids = navItems().map((item) => item.id);
  assert.equal(new Set(ids).size, ids.length, "ids are unique");
  assert.equal(navItem("github").href, "wiki.html?view=github", "GitHub is a source, not a slot");
  // Agent accounts and access shipped with ADR 0004: real tabs now.
  for (const id of ["agents", "access"]) {
    const item = navItem(id);
    assert.equal(item.href, `settings.html#${id}`, `${id} is a settings tab`);
    assert.equal(item.soon, undefined, `${id} is no longer a slot`);
  }
  for (const item of navItems()) assert.ok(ICONS[item.icon], `${item.id} has an icon`);
});

test("settings tabs: every section belongs to exactly one tab, unknown hashes are Models", () => {
  const seen = new Set();
  for (const sections of Object.values(SETTINGS_TABS)) {
    for (const id of sections) {
      assert.ok(!seen.has(id), `${id} is in one tab`);
      seen.add(id);
    }
  }
  const html = readFileSync(new URL("../settings.html", import.meta.url), "utf8");
  for (const id of seen) assert.match(html, new RegExp(`id="${id}"`), `settings.html has #${id}`);
  assert.equal(settingsTab(""), "models");
  assert.equal(settingsTab("#nope"), "models");
  assert.equal(settingsTab("#connect"), "integrations");
  assert.equal(settingsTab("account"), "account");
});

test("pageFile: clean URLs and the root are the documents they serve", () => {
  assert.equal(pageFile("/"), "index.html");
  assert.equal(pageFile(""), "index.html");
  assert.equal(pageFile("/wiki"), "wiki.html");
  assert.equal(pageFile("/app/settings.html"), "settings.html");
});

test("the view a page is showing wins over its address", () => {
  const where = { pathname: "/wiki.html", search: "?view=sessions&source=01H", hash: "" };
  assert.equal(activeId(where), "sessions");
  assert.equal(activeId({ pathname: "/wiki.html", search: "" }, "ask"), "ask");
  assert.equal(activeId({ pathname: "/wiki.html", search: "?slug=a" }, "editor"), "pages");
  assert.equal(activeId({ pathname: "/wiki.html", search: "" }, "new"), "pages");
  assert.equal(activeId({ pathname: "/wiki.html", search: "" }, "imports"), "imports");
});

test("breadcrumbs file a page where the sidebar does", () => {
  assert.deepEqual(pageCrumbs("kestrel", "Kestrel routes"), [
    { label: "Pages", href: "wiki.html" },
    { label: "Kestrel routes" },
  ]);
  assert.deepEqual(pageCrumbs("agent-log-colonizer-a15fba01", "EarthOS.world · Colony a15fba01"), [
    { label: "Agent sessions", href: "wiki.html?view=sessions" },
    { label: "EarthOS.world · Colony a15fba01" },
  ]);
  assert.deepEqual(pageCrumbs("coding-agents--claude--demo--decisions", "Decisions"), [
    { label: "Pages", href: "wiki.html" },
    { label: "claude" },
    { label: "demo" },
    { label: "Decisions" },
  ]);
  assert.deepEqual(pageCrumbs("lonely", ""), [{ label: "Pages", href: "wiki.html" }, { label: "lonely" }]);
  assert.deepEqual(transcriptCrumbs("claude session 1a2b"), [
    { label: "Agent sessions", href: "wiki.html?view=sessions" },
    { label: "claude session 1a2b" },
  ]);
});

test("palette: an empty query lists every destination, then the actions", () => {
  const rows = paletteOptions("", { signedIn: true });
  const goTo = rows.filter((row) => row.group === "Go to").map((row) => row.label);
  assert.deepEqual(goTo, navItems().filter((item) => item.href).map((item) => item.label));
  assert.deepEqual(
    rows.filter((row) => row.group === "Actions").map((row) => row.label),
    ["New page", "Switch theme", "Sign out"],
  );
  assert.ok(!rows.some((row) => row.group === "Ask"), "nothing to ask yet");
  // Signed out: no account actions.
  const out = paletteOptions("", { signedIn: false }).filter((row) => row.group === "Actions");
  assert.deepEqual(out.map((row) => row.label), ["Switch theme"]);
});

test("palette: a query narrows destinations, lists page hits, and offers to ask", () => {
  // Dreaming's hint says "its facts", so it matches too, after the label hit.
  assert.deepEqual(matchDestinations("fac").map((item) => item.id), ["facts", "dreaming"]);
  assert.deepEqual(matchDestinations("token").map((item) => item.id), ["tokens"]);
  // A word from the hint finds it too.
  assert.ok(matchDestinations("budgets").some((item) => item.id === "limits"));
  const rows = paletteOptions("kestrel routes", {
    signedIn: true,
    pages: [
      { title: "Kestrel routes", url: "https://app.livingbrain.wiki/wiki?slug=kestrel-routes&scope=shared", snippet: "edge traffic" },
      { title: "No slug", url: "https://example.com/elsewhere" },
    ],
  });
  const pages = rows.filter((row) => row.group === "Pages");
  assert.deepEqual(pages.map((row) => row.href), ["wiki.html?slug=kestrel-routes"]);
  const ask = rows.at(-1);
  assert.equal(ask.group, "Ask");
  assert.equal(ask.href, "wiki.html?view=ask&q=kestrel%20routes");
  assert.match(ask.label, /Ask the brain: “kestrel routes”/);
  // Signed out, there is no Ask row: the question would only be refused.
  assert.ok(!paletteOptions("kestrel", { signedIn: false }).some((row) => row.group === "Ask"));
});

test("palette helpers: the search path, the hit's slug, the ask address", () => {
  assert.equal(paletteSearchPath("  a b "), "/v1/search?q=a%20b&limit=6");
  assert.equal(hitSlug("https://app.livingbrain.wiki/wiki?slug=q3-plan&scope=user-01HX"), "q3-plan");
  assert.equal(hitSlug("https://livingbrain.wiki/brain/shared/kestrel-routes"), "kestrel-routes");
  assert.equal(hitSlug("https://app.livingbrain.wiki/wiki?slug=Not%20A%20Slug"), null);
  assert.equal(hitSlug(""), null);
  assert.equal(askHref(""), "wiki.html?view=ask");
  assert.equal(askHref("what?"), "wiki.html?view=ask&q=what%3F");
});

test("the four pages carry one shell with the same ids, and the theme script is untouched", () => {
  const SHELL_IDS = [
    "nav-toggle",
    "ws-switch",
    "ws-button",
    "ws-menu",
    "palette-open",
    "theme-toggle",
    "session-bar",
    "session-who",
    "user-button",
    "user-menu",
    "header-signout",
    "header-signin",
    "app-sidebar",
    "app-nav",
    "nav-collapse",
    "nav-scrim",
    "main",
  ];
  const theme = (html) => /<script>([\s\S]*?)<\/script>/.exec(html)[1];
  const pages = ["index.html", "wiki.html", "settings.html", "brain.html"].map((name) => [
    name,
    readFileSync(new URL(`../${name}`, import.meta.url), "utf8"),
  ]);
  for (const [name, html] of pages) {
    for (const id of SHELL_IDS) {
      assert.equal(html.split(`id="${id}"`).length - 1, 1, `${name} has exactly one #${id}`);
    }
    assert.match(html, /<nav id="app-nav" class="sidebar__nav" aria-label="Main">/, name);
    assert.match(html, /<a class="skip" href="#main">/, name);
    assert.equal(theme(html), theme(pages[0][1]), `${name}'s theme script is byte-identical`);
    // Every local script and stylesheet the page names exists.
    for (const [, ref] of html.matchAll(/(?:src|href)="(assets\/[^"]+)"/g)) {
      assert.ok(existsSync(new URL(`../${ref}`, import.meta.url)), `${name} → ${ref}`);
    }
  }
});

test("the old header nav and the drawer's link mirror are gone; the views have their own heads", () => {
  const wiki = readFileSync(new URL("../wiki.html", import.meta.url), "utf8");
  assert.doesNotMatch(wiki, /class="nav"/, "no header link row");
  assert.doesNotMatch(wiki, /drawer-signin|drawer-signout|wiki-drawer__nav/, "the drawer is the TOC only");
  for (const view of ["home-view", "ask-view", "sessions-view", "facts-view", "coding-agents-view", "imports-view"]) {
    const start = wiki.indexOf(`id="${view}"`);
    assert.ok(start > 0, view);
    const head = wiki.slice(start, start + 400);
    assert.match(head, /class="page-head"/, `${view} opens with a page head`);
    assert.match(head, /class="eyebrow">(Brain|Sources)</, `${view} names its group`);
  }
  assert.match(wiki, /id="page-crumbs" class="crumbs" aria-label="Breadcrumb"/);
  assert.match(wiki, /id="transcript-crumbs" class="crumbs" aria-label="Breadcrumb"/);
});

test("the service worker caches the shell's modules", () => {
  // The built worker's precache list is generated from the pages' module graph
  // (tools/stamp-assets.mjs), so the shell's modules are in it by construction.
  const { names, precache } = plan(fileURLToPath(new URL("..", import.meta.url)));
  assert.ok(precache.includes("./brain.html"));
  for (const file of ["shell.js", "nav.js", "brain.js", "brain-render.js", "graph.js", "brain.css"]) {
    assert.ok(precache.includes(`./${names.get(`assets/${file}`)}`), file);
  }
});
