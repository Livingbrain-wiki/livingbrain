// The app's information architecture, as data (docs/ux-navigation.md).
//
// One list drives the sidebar, the command palette and the breadcrumbs, and
// `tests/nav.test.mjs` pins it: every address the app ever published — the
// API's citation URLs, the Mac app's links, links pasted into pages —
// resolves to one of these destinations. Pure: no DOM, no fetch, so Node
// imports it directly.

/**
 * The sidebar's groups, in order. An item is `{ id, label, href, icon,
 * hint }`; an item with `soon` instead of an `href` is a reserved slot,
 * shown but never a link (GitHub until its UI lands, the #180 slots).
 */
export const NAV = Object.freeze([
  {
    id: "brain",
    label: "Brain",
    items: [
      { id: "home", label: "Home", href: "index.html", icon: "home", hint: "Your workspace at a glance" },
      { id: "ask", label: "Ask the brain", href: "wiki.html?view=ask", icon: "ask", hint: "Search pages, or ask a question" },
      { id: "pages", label: "Pages", href: "wiki.html", icon: "pages", hint: "Pinned and recent pages" },
      { id: "facts", label: "Facts", href: "wiki.html?view=facts", icon: "facts", hint: "What is true, and since when" },
      { id: "map", label: "Brain map", href: "brain.html", icon: "map", hint: "Your brain as a 3D graph" },
    ],
  },
  {
    id: "sources",
    label: "Sources",
    items: [
      { id: "sessions", label: "Agent sessions", href: "wiki.html?view=sessions", icon: "sessions", hint: "Synced coding-agent sessions" },
      { id: "coding-agents", label: "Coding agents", href: "wiki.html?view=coding-agents", icon: "folder", hint: "Pages compiled from your sessions" },
      { id: "github", label: "GitHub", soon: "Connecting GitHub is coming soon.", icon: "github" },
      { id: "imports", label: "Imports", href: "wiki.html?view=imports", icon: "import", hint: "Files imported into the brain" },
    ],
  },
  {
    id: "workspace",
    label: "Workspace",
    items: [
      { id: "members", label: "Members", href: "settings.html#members", icon: "members", hint: "Who is in this workspace" },
      { id: "agents", label: "Agent accounts", soon: "Agent accounts are coming soon.", icon: "agent" },
      { id: "access", label: "Access & audit", soon: "Access grants and the audit log are coming soon.", icon: "lock" },
    ],
  },
  {
    id: "settings",
    label: "Settings",
    items: [
      { id: "models", label: "Models", href: "settings.html#models", icon: "model", hint: "The models the brain thinks with" },
      { id: "limits", label: "Usage & limits", href: "settings.html#limits", icon: "gauge", hint: "Budgets and spend" },
      { id: "integrations", label: "Integrations", href: "settings.html#integrations", icon: "plug", hint: "MCP, the CLI and what is coming" },
      { id: "tokens", label: "API tokens", href: "settings.html#tokens", icon: "key", hint: "Tokens for agents and the CLI" },
      { id: "account", label: "Account", href: "settings.html#account", icon: "user", hint: "You, your workspaces, sign out" },
    ],
  },
]);

/** Every destination with its group, in sidebar order. */
export function navItems(nav = NAV) {
  const out = [];
  for (const group of nav) {
    for (const item of group.items) out.push({ ...item, group: group.id, groupLabel: group.label });
  }
  return out;
}

/** One destination by id, or null. */
export function navItem(id, nav = NAV) {
  return navItems(nav).find((item) => item.id === id) || null;
}

/**
 * The settings tabs and the section ids each shows. The old anchors stay
 * addressable: `#connect`, `#soon` and `#stack` are inside Integrations.
 */
export const SETTINGS_TABS = Object.freeze({
  models: ["models"],
  limits: ["limits"],
  integrations: ["connect", "soon", "stack"],
  tokens: ["tokens"],
  members: ["members"],
  account: ["account"],
});

/** A `settings.html` hash (with or without `#`) to the tab that holds it;
 * anything unknown is the first tab, Models. */
export function settingsTab(hash) {
  const id = String(hash || "").replace(/^#/, "").trim();
  if (Object.prototype.hasOwnProperty.call(SETTINGS_TABS, id)) return id;
  for (const [tab, sections] of Object.entries(SETTINGS_TABS)) {
    if (sections.includes(id)) return tab;
  }
  return "models";
}

/** The `wiki.html` views and the sidebar destination each belongs to. */
const WIKI_VIEWS = Object.freeze({
  "": "pages",
  home: "pages",
  new: "pages",
  ask: "ask",
  facts: "facts",
  sessions: "sessions",
  transcript: "sessions",
  "coding-agents": "coding-agents",
  imports: "imports",
});

/** The file name of a path: `/`, `/index.html` and `` are `index.html`, and
 * the clean URLs Workers static assets serves (`/wiki`) gain `.html`. */
export function pageFile(pathname) {
  const last = String(pathname || "").split("/").pop() || "";
  if (!last) return "index.html";
  return last.endsWith(".html") ? last : `${last}.html`;
}

/**
 * Which sidebar destination an address is: `{ pathname, search, hash }` (a
 * `location`, or the parts of a URL) to an item id, or null for an address
 * that is none of them. `view` overrides the query's view — `wiki.js` passes
 * the view it is actually showing, which a page link's `?slug=` implies.
 */
export function activeId({ pathname = "", search = "", hash = "" } = {}, view = null) {
  const file = pageFile(pathname);
  if (file === "index.html") return "home";
  if (file === "brain.html") return "map";
  if (file === "settings.html") return settingsTab(hash);
  if (file === "wiki.html") {
    const params = new URLSearchParams(search);
    let current = view;
    if (current == null) {
      if (params.get("slug")) return pageIsCompiled(params.get("slug")) ? "coding-agents" : "pages";
      current = params.get("view") || "";
      if (current === "sessions" && params.get("source")) current = "transcript";
    }
    if (current === "page" || current === "editor") {
      return pageIsCompiled(params.get("slug") || "") ? "coding-agents" : "pages";
    }
    return Object.prototype.hasOwnProperty.call(WIKI_VIEWS, current) ? WIKI_VIEWS[current] : "pages";
  }
  return null;
}

/** Whether a slug is one of the compiled coding-agents pages
 * (`coding-agents--…`), which live under Sources, not Pages. */
export function pageIsCompiled(slug) {
  return /^coding-agents--[a-z0-9-]/.test(String(slug || ""));
}

/**
 * The breadcrumb trail for a wiki page: `[{label, href}]`, the last entry
 * the page itself (no `href`). A compiled page is filed under Coding agents
 * and its folders; any other page under Pages.
 */
export function pageCrumbs(slug, title) {
  const name = String(title || slug || "Untitled");
  const value = String(slug || "");
  if (pageIsCompiled(value)) {
    const parts = value.split("--").slice(1);
    const crumbs = [{ label: "Coding agents", href: "wiki.html?view=coding-agents" }];
    // The folders between the collection and the page (agent, project).
    for (const part of parts.slice(0, -1)) crumbs.push({ label: part });
    crumbs.push({ label: name });
    return crumbs;
  }
  return [{ label: "Pages", href: "wiki.html" }, { label: name }];
}

/** The breadcrumb trail for a transcript. */
export function transcriptCrumbs(title) {
  return [
    { label: "Agent sessions", href: "wiki.html?view=sessions" },
    { label: String(title || "Agent session") },
  ];
}

/** Lowercased words of a palette query. */
function words(query) {
  return String(query || "")
    .toLowerCase()
    .split(/\s+/)
    .filter(Boolean);
}

/**
 * The destinations a palette query matches, best first: every word must
 * appear in the label, the group or the hint; a label that starts with the
 * query ranks first. Reserved slots are left out — there is nowhere to go.
 * An empty query lists everything in sidebar order.
 */
export function matchDestinations(query, nav = NAV) {
  const list = navItems(nav).filter((item) => item.href);
  const terms = words(query);
  if (!terms.length) return list;
  const scored = [];
  list.forEach((item, at) => {
    const label = item.label.toLowerCase();
    const hay = `${label} ${item.groupLabel.toLowerCase()} ${String(item.hint || "").toLowerCase()}`;
    if (!terms.every((term) => hay.includes(term))) return;
    const joined = terms.join(" ");
    const score = label.startsWith(joined) ? 0 : label.includes(joined) ? 1 : 2;
    scored.push({ item, score, at });
  });
  scored.sort((a, b) => a.score - b.score || a.at - b.at);
  return scored.map((entry) => entry.item);
}

/** The Ask view's address for a question. */
export function askHref(question) {
  const q = String(question || "").trim();
  return q ? `wiki.html?view=ask&q=${encodeURIComponent(q)}` : "wiki.html?view=ask";
}

/** How many page hits the palette asks for. */
export const PALETTE_SEARCH_LIMIT = 6;

/** The palette's search request: the same route as the Ask box. */
export function paletteSearchPath(query) {
  return `/v1/search?q=${encodeURIComponent(String(query || "").trim())}&limit=${PALETTE_SEARCH_LIMIT}`;
}

/** A search hit's page slug, from the citation URL the API returns
 * (`…/wiki?slug=<slug>&scope=…`, or the website's `/brain/<scope>/<slug>`). */
export function hitSlug(url) {
  const value = String(url || "");
  const query = value.split("?")[1];
  if (query) {
    const slug = new URLSearchParams(query.split("#")[0]).get("slug");
    if (slug && /^[a-z0-9-]{1,128}$/.test(slug)) return slug;
  }
  const match = /\/brain\/[^/?#]+\/([a-z0-9-]{1,128})(?:[?#]|$)/.exec(value);
  return match ? match[1] : null;
}

/** The palette's commands that are not destinations. */
const ACTIONS = Object.freeze([
  { id: "new-page", label: "New page", href: "wiki.html?view=new", hint: "create write" },
  { id: "theme", label: "Switch theme", action: "theme", hint: "dark light appearance" },
  { id: "signout", label: "Sign out", action: "signout", hint: "log out leave" },
]);

/**
 * The palette's rows for a query, in display order, each `{ group, label,
 * detail?, href? | action? }`:
 *
 * - **Go to** — the destinations the query matches (all of them when empty);
 * - **Pages** — the page hits the search returned, each opening its page;
 * - **Actions** — New page, Switch theme, Sign out (the account ones only
 *   signed in);
 * - **Ask** — "Ask the brain: …" with the typed question, signed in.
 */
export function paletteOptions(query, { pages = [], signedIn = false } = {}) {
  const q = String(query || "").trim();
  const goTo = matchDestinations(q)
    .slice(0, q ? 6 : Infinity)
    .map((item) => ({ group: "Go to", label: item.label, detail: item.groupLabel, href: item.href }));
  const terms = words(q);
  const actions = [];
  for (const action of ACTIONS) {
    if ((action.id === "signout" || action.id === "new-page") && !signedIn) continue;
    const hay = `${action.label} ${action.hint}`.toLowerCase();
    if (terms.length && !terms.every((term) => hay.includes(term))) continue;
    actions.push({ group: "Actions", label: action.label, href: action.href, action: action.action });
  }
  // An action named by the query ("sign out") outranks a destination that
  // only mentions it in passing (Account's "…, sign out").
  const named = q && actions.some((row) => row.label.toLowerCase().startsWith(q.toLowerCase()));
  const namedDestination = q && goTo.some((row) => row.label.toLowerCase().startsWith(q.toLowerCase()));
  const rows = named && !namedDestination ? [...actions] : [...goTo];
  for (const hit of Array.isArray(pages) ? pages : []) {
    const slug = hitSlug(hit && hit.url);
    if (!slug) continue;
    rows.push({
      group: "Pages",
      label: String((hit && hit.title) || slug),
      detail: String((hit && hit.snippet) || "").slice(0, 90),
      href: `wiki.html?slug=${encodeURIComponent(slug)}`,
    });
  }
  if (named && !namedDestination) rows.splice(actions.length, 0, ...goTo);
  else rows.push(...actions);
  if (q && signedIn) {
    rows.push({ group: "Ask", label: `Ask the brain: “${q}”`, detail: "An answer with its sources", href: askHref(q) });
  }
  return rows;
}

/**
 * Small line icons for the sidebar, as SVG path data on a 24-unit grid
 * (stroked, no fill). Drawn with `createElementNS`, never parsed markup.
 */
export const ICONS = Object.freeze({
  home: ["M4 11l8-7 8 7", "M6 10v10h12V10"],
  ask: ["M11 4a7 7 0 1 0 0 14 7 7 0 0 0 0-14z", "M16.5 16.5L21 21"],
  pages: ["M7 3h7l5 5v13H7z", "M14 3v5h5", "M10 13h6", "M10 17h6"],
  facts: ["M5 5h14v14H5z", "M9 9h6", "M9 13h6", "M9 17h3"],
  map: ["M12 4a3 3 0 1 0 0 .01", "M5 16a3 3 0 1 0 0 .01", "M19 16a3 3 0 1 0 0 .01", "M10.5 9.5L6.5 14", "M13.5 9.5l4 4.5", "M8 17h8"],
  sessions: ["M4 6h16v10H4z", "M8 20h8", "M7 10l2 2-2 2", "M11 14h4"],
  folder: ["M3 7h7l2 2h9v10H3z"],
  github: ["M9 19c-4 1.5-4-2-6-2", "M15 21v-3.5c0-1 .1-1.4-.5-2 2.8-.3 5.5-1.4 5.5-6a4.6 4.6 0 0 0-1.3-3.2 4.2 4.2 0 0 0-.1-3.2s-1.1-.3-3.5 1.3a12 12 0 0 0-6.2 0C6.5 2.8 5.4 3.1 5.4 3.1a4.2 4.2 0 0 0-.1 3.2A4.6 4.6 0 0 0 4 9.5c0 4.6 2.7 5.7 5.5 6-.6.6-.6 1.2-.5 2V21"],
  import: ["M12 3v12", "M7 10l5 5 5-5", "M5 21h14"],
  members: ["M9 11a3 3 0 1 0 0-6 3 3 0 0 0 0 6z", "M3 20a6 6 0 0 1 12 0", "M16 5a3 3 0 0 1 0 6", "M18 14a5 5 0 0 1 3 6"],
  agent: ["M6 8h12v10H6z", "M12 4v4", "M9 13h.01", "M15 13h.01", "M10 16h4"],
  lock: ["M6 11h12v9H6z", "M8 11V8a4 4 0 0 1 8 0v3"],
  model: ["M12 3l8 4.5v9L12 21l-8-4.5v-9z", "M12 12l8-4.5", "M12 12v9", "M12 12L4 7.5"],
  gauge: ["M4 18a8 8 0 1 1 16 0", "M12 18l4-6"],
  plug: ["M9 3v5", "M15 3v5", "M6 8h12v3a6 6 0 0 1-12 0z", "M12 17v4"],
  key: ["M8 14a4 4 0 1 0 0-.01", "M11 11l9-9", "M17 5l2 2", "M15 7l2 2"],
  user: ["M12 12a4 4 0 1 0 0-8 4 4 0 0 0 0 8z", "M4 21a8 8 0 0 1 16 0"],
  collapse: ["M15 6l-6 6 6 6"],
  expand: ["M9 6l6 6-6 6"],
});
