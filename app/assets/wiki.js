// The wiki page: home (list + pins, and the compiled `coding-agents/`
// folder), a page, its editor, the new-page form, the agent-sessions list
// with each session's compile status and the queue card, one session's
// transcript, the ask panel and header search (with its project filter).
//
// `app.js` imports `wikiPage` from here and its boot dispatches it for
// `body[data-page="wiki"]`, so this file is the one module the wiki page runs
// — the same functions these tests drive. It never sends an Authorization
// header (see the header comment in `app.js`): the session is the cookie, and
// a `401` from the API is the signed-out state, shown as such, never an error
// dialog.
//
// Everything that reaches the document goes through `mountNodes` /
// `createElement` + `textContent` — no `innerHTML` anywhere, like the rest of
// `app/`.

import {
  $,
  $$,
  ApiError,
  mountNodes,
  notice,
  renderInto,
} from "./app.js";
import {
  renderBacklinks,
  renderCitations,
  renderMarkdown,
  isSafeHref,
} from "./markdown.js";
import { applyHeader, checkSession, signOutRequest } from "./session.js";
import { factsDescription, factsPath } from "./facts.js";

/** Where the per-browser list of pinned `scope/slug` keys lives. */
const PIN_KEY = "lb-wiki-pinned";

/** The server's slug rule (crates/livingbrain-api): 1..128 of [a-z0-9-]. */
export const SLUG_PATTERN = /^[a-z0-9-]{1,128}$/;

/** The message the new-page form shows when a slug fails that rule. */
export const SLUG_MESSAGE =
  "Use lowercase letters, digits and hyphens — 1 to 128 characters.";

/** Header search: keystrokes settle for this long before a request goes out. */
const SEARCH_DEBOUNCE_MS = 200;

/** Header search: the API caps `limit` at 25; ten fits the panel. */
const SEARCH_LIMIT = 10;

/** The server's project rule (`livingbrain_pages::is_project`): 1..64 of
 * ASCII letters, digits, `.`, `_` and `-`, starting with a letter or digit. */
export const PROJECT_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/;

/** Agent sessions: one page of the list. The API caps `limit` at 100. */
const SESSIONS_LIMIT = 50;

/** What a `TreeWalker` over text nodes asks for: `NodeFilter.SHOW_TEXT`,
 * spelled out for DOM stubs running under Node, which has no `NodeFilter`. */
const SHOW_TEXT = 4;

/* --------------------------------------------------------- pure helpers */

/** Free text to a slug: lowercase, `[a-z0-9-]`, at most 128 chars, no dashes
 * at either end. Answers "" when nothing survives — callers decide what an
 * empty slug means for them. */
export function slugify(value) {
  return String(value == null ? "" : value)
    .trim()
    .toLowerCase()
    .replace(/['’]/g, "")
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+/, "")
    .slice(0, 128)
    .replace(/-+$/, "");
}

/** True when `value` is exactly what the server accepts as a slug. */
export function validSlug(value) {
  return typeof value === "string" && SLUG_PATTERN.test(value);
}

/** True when `value` is exactly what the server accepts as a project. */
export function validProject(value) {
  return typeof value === "string" && PROJECT_PATTERN.test(value);
}

/** The header search's request: `q` and `limit`, plus `project` when one is
 * chosen. With no project the URL is exactly what it always was. */
export function searchPath(query, project, limit = SEARCH_LIMIT) {
  let path = `/v1/search?q=${encodeURIComponent(query)}&limit=${limit}`;
  if (validProject(project)) path += `&project=${encodeURIComponent(project)}`;
  return path;
}

/** One page of the agent-sessions list: `kind=agent_log`, the project when
 * one is chosen, and the previous page's cursor when there is one. */
export function sessionsPath({ project = "", cursor = "", limit = SESSIONS_LIMIT } = {}) {
  const params = new URLSearchParams({ kind: "agent_log", limit: String(limit) });
  if (validProject(project)) params.set("project", project);
  if (cursor) params.set("cursor", cursor);
  return `/v1/sources?${params.toString()}`;
}

/** One source, body included: the transcript view's request. */
export function sourcePath(id) {
  return `/v1/sources/${encodeURIComponent(String(id == null ? "" : id))}`;
}

/** The address of one session's transcript. */
export function transcriptHref(id) {
  return `wiki.html?view=sessions&source=${encodeURIComponent(String(id == null ? "" : id))}`;
}

/** The newest row per session: a re-sync after an upgrade files a new
 * source beside the old one under the same `path`, and the listing is
 * newest first, so the first row of each path is the one to open. */
export function latestSessions(rows) {
  const seen = new Set();
  const out = [];
  for (const row of rows || []) {
    const key = (row && row.path) || (row && row.id) || "";
    if (seen.has(key)) continue;
    seen.add(key);
    out.push(row);
  }
  return out;
}

/** An `agent_log` source body to its record, or null when it is not one.
 * Version 1 and version 2 records read alike: version 1 simply has no tool
 * inputs or results, no reasoning and no events. */
export function parseSessionRecord(body) {
  let record;
  try {
    record = JSON.parse(String(body == null ? "" : body));
  } catch {
    return null;
  }
  if (!record || typeof record !== "object" || !Array.isArray(record.turns)) return null;
  return record;
}

/** A transcript's numbers, for its header line. */
export function sessionStats(record) {
  const turns = (record && record.turns) || [];
  let user = 0;
  let assistant = 0;
  let tools = 0;
  for (const turn of turns) {
    if (turn.role === "user") user += 1;
    if (turn.role === "assistant" && String(turn.text || "").trim()) assistant += 1;
    tools += Array.isArray(turn.tool_calls) ? turn.tool_calls.length : 0;
  }
  return { user, assistant, tools };
}

/** `2026-10-01T09:30:05.123Z` as `2026-10-01 09:30:05`. */
function shortTime(at) {
  return String(at || "").replace(/Z$/, "").replace(/\.\d+$/, "").replace("T", " ");
}

/** How many lines a text has, for an output's summary. */
function lineCount(text) {
  const value = String(text || "").replace(/\n$/, "");
  return value ? value.split("\n").length : 0;
}

/** One tool call as node descriptions: name, file and outcome on a line,
 * the input fenced, the result folded away when it is long. */
function callNodes(call) {
  const head = [el("code", [tx(call.name || "tool")])];
  if (call.path) head.push(tx(" "), el("code", [tx(call.path)], { class: "transcript-call__path" }));
  const outcome = [];
  if (call.status) outcome.push(call.status);
  if (call.exit_code != null) outcome.push(`exit ${call.exit_code}`);
  if (outcome.length) {
    head.push(
      el("span", [tx(` · ${outcome.join(" · ")}`)], {
        class: `transcript-call__status${call.status === "error" ? " is-bad" : ""}`,
      }),
    );
  }
  const children = [el("div", head, { class: "transcript-call__head" })];
  if (call.input) {
    const lang = call.format === "shell" ? "sh" : call.format || "text";
    children.push(el("pre", [el("code", [tx(call.input)])], { "data-lang": lang }));
  }
  if (call.output) {
    const lines = lineCount(call.output);
    const attrs = { class: "transcript-call__output" };
    if (lines <= 12) attrs.open = "";
    children.push(
      el("details", [
        el("summary", [tx(`Output · ${lines} line${lines === 1 ? "" : "s"}`)]),
        el("pre", [el("code", [tx(call.output)])]),
      ], attrs),
    );
  }
  return el("div", children, { class: "transcript-call" });
}

/** One turn as a node description, or null when it shows nothing. */
function turnNodes(turn) {
  const role = turn.role || "assistant";
  const text = String(turn.text || "");
  const at = turn.at ? shortTime(turn.at) : "";
  if (role === "event") {
    return el("p", [tx(`— ${text}${at ? ` · ${at}` : ""} —`)], { class: "transcript-event" });
  }
  const label = { user: "User", assistant: "Assistant", reasoning: "Reasoning" }[role] || role;
  const head = el("div", [el("strong", [tx(label)]), tx(at ? ` · ${at}` : "")], {
    class: "transcript-turn__head",
  });
  const body = [];
  if (role === "reasoning") {
    if (!text.trim()) return null;
    body.push(
      el("details", [el("summary", [tx("Reasoning summary")]), el("p", [tx(text)], { class: "transcript-text" })], {
        class: "transcript-reasoning",
      }),
    );
    return el("div", body, { class: "transcript-turn transcript-turn--reasoning" });
  }
  if (text.trim()) {
    // The person's words as they typed them; the agent's as the markdown
    // they are — through the same renderer as any page, no HTML.
    if (role === "assistant") body.push(el("div", renderMarkdown(text), { class: "transcript-md" }));
    else body.push(el("p", [tx(text)], { class: "transcript-text" }));
  }
  for (const call of Array.isArray(turn.tool_calls) ? turn.tool_calls : []) {
    body.push(callNodes(call || {}));
  }
  if (!body.length) return null;
  return el("div", [text.trim() ? head : null, ...body].filter(Boolean), {
    class: `transcript-turn transcript-turn--${role}`,
  });
}

/** The whole transcript as node descriptions: the header facts, then every
 * turn in order. Pure, so the tests read it without a page. */
export function transcriptNodes(record) {
  const stats = sessionStats(record);
  const totals = record.totals || {};
  const facts = [
    ["Agent", record.agent],
    ["Repo", record.repo],
    ["Branch", record.branch],
    [
      "When",
      record.started_at && record.ended_at && record.started_at !== record.ended_at
        ? `${shortTime(record.started_at)} to ${shortTime(record.ended_at)}`
        : shortTime(record.started_at || record.ended_at || ""),
    ],
    ["Models", Array.isArray(record.models) ? record.models.join(", ") : ""],
    ["Messages", `${stats.user} user, ${stats.assistant} assistant`],
    ["Tool calls", String(stats.tools)],
    [
      "Tokens",
      totals.input_tokens != null
        ? `${totals.input_tokens} in, ${totals.output_tokens || 0} out, ${totals.cache_read_tokens || 0} cache read`
        : "",
    ],
    ["Cost", totals.cost_usd != null ? `$${Number(totals.cost_usd).toFixed(4)}` : ""],
    ["Detail", record.detail || (record.schema_version === 1 ? "summary (version 1 record)" : "")],
  ].filter(([, value]) => value);
  const nodes = [
    el(
      "ul",
      facts.map(([name, value]) => el("li", [el("strong", [tx(`${name}: `)]), tx(value)])),
      { class: "transcript-facts" },
    ),
  ];
  if (record.truncated) {
    nodes.push(
      el(
        "p",
        [
          tx(
            record.omitted_turns
              ? `${record.omitted_turns} turn(s) were left out to fit the upload size cap.`
              : "Part of this session was left out to fit the upload size cap.",
          ),
        ],
        { class: "transcript-note" },
      ),
    );
  }
  for (const turn of record.turns || []) {
    const node = turnNodes(turn || {});
    if (node) nodes.push(node);
  }
  return nodes;
}

/* ------------------------------------------------- the coding-agents wiki */

/** The compiled wiki's collection (crates/livingbrain-compiler, layout.rs):
 * a slug has no `/`, so its folder is spelled with `--` between segments —
 * `coding-agents--claude--chi-infra--decisions` is
 * `coding-agents/claude/chi-infra/decisions`. */
export const CODING_AGENTS = "coding-agents";

/** The folder the cross-agent project rollups live in. */
const ROLLUP = "projects";

/** A collection slug to where it sits, or null for any other slug:
 * `{ kind: "top" | "agent" | "rollup" | "project", path, agent?, project?, page? }`. */
export function codingAgentsPlace(slug) {
  if (!validSlug(slug)) return null;
  const parts = String(slug).split("--");
  if (parts[0] !== CODING_AGENTS || parts.length < 2 || parts.length > 4) return null;
  const rest = parts.slice(1);
  if (rest.some((part) => !part || part.startsWith("-") || part.endsWith("-"))) return null;
  const path = [CODING_AGENTS, ...rest];
  if (rest.length === 1) return { kind: "top", path, page: rest[0] };
  if (rest.length === 2 && rest[0] === ROLLUP) return { kind: "rollup", path, project: rest[1] };
  if (rest.length === 2) return { kind: "agent", path, agent: rest[0], page: rest[1] };
  if (rest[0] === ROLLUP) return null;
  return { kind: "project", path, agent: rest[0], project: rest[1], page: rest[2] };
}

/** The order pages sit in inside a folder: the index first, then the fixed
 * pages, then topics alphabetically. */
const PAGE_ORDER = ["index", "log", "schema", "lint", "patterns", "decisions", "learnings", "architecture", "open-questions"];

function pageRank(page) {
  const at = PAGE_ORDER.indexOf(page);
  return at === -1 ? PAGE_ORDER.length : at;
}

function byPage(a, b) {
  return pageRank(a.place.page) - pageRank(b.place.page) || a.place.page.localeCompare(b.place.page);
}

/** The collection's pages out of a page listing, as a folder tree:
 * `{ top, agents: [{ agent, pages, projects: [{ project, pages }] }], rollups }`,
 * each page `{ row, place }`. Rows outside the collection are left out. */
export function codingAgentsTree(rows) {
  const top = [];
  const rollups = [];
  const agents = new Map();
  const agent = (name) => {
    if (!agents.has(name)) agents.set(name, { agent: name, pages: [], projects: new Map() });
    return agents.get(name);
  };
  for (const row of rows || []) {
    const place = codingAgentsPlace(row && row.slug);
    if (!place) continue;
    const entry = { row, place };
    if (place.kind === "top") top.push(entry);
    else if (place.kind === "rollup") rollups.push(entry);
    else if (place.kind === "agent") agent(place.agent).pages.push(entry);
    else {
      const folder = agent(place.agent);
      if (!folder.projects.has(place.project)) {
        folder.projects.set(place.project, { project: place.project, pages: [] });
      }
      folder.projects.get(place.project).pages.push(entry);
    }
  }
  top.sort(byPage);
  rollups.sort((a, b) => a.place.project.localeCompare(b.place.project));
  return {
    top,
    rollups,
    agents: [...agents.values()]
      .sort((a, b) => a.agent.localeCompare(b.agent))
      .map((folder) => ({
        agent: folder.agent,
        pages: folder.pages.sort(byPage),
        projects: [...folder.projects.values()]
          .sort((a, b) => a.project.localeCompare(b.project))
          .map((project) => ({ project: project.project, pages: project.pages.sort(byPage) })),
      })),
  };
}

/** The compile queue's status route, optionally for one source. */
export function queuePath(source = "") {
  return source
    ? `/v1/coding-agents/queue?source=${encodeURIComponent(source)}`
    : "/v1/coding-agents/queue?limit=100";
}

/** What each compile status says to a person. */
export const JOB_LABELS = {
  queued: "Queued",
  processing: "Processing",
  done: "Compiled",
  failed: "Failed",
  "skipped-no-model": "No model",
  "over-budget": "Over budget",
  superseded: "Superseded",
  skipped: "Skipped",
  "waiting-runner": "Waiting for your device",
};

export function jobLabel(status) {
  return JOB_LABELS[status] || String(status || "");
}

/** The status route's counts as one line, busiest first, zeros left out. */
export function queueSummary(counts) {
  const parts = [];
  for (const status of Object.keys(JOB_LABELS)) {
    const n = Number((counts && counts[status]) || 0);
    if (n > 0) parts.push(`${n} ${jobLabel(status).toLowerCase()}`);
  }
  return parts.length ? parts.join(" · ") : "Nothing queued";
}

/** The newest job per source id. */
export function jobsBySource(jobs) {
  const out = new Map();
  for (const job of jobs || []) {
    if (job && job.source_id && !out.has(job.source_id)) out.set(job.source_id, job);
  }
  return out;
}

/** A compiled page's slug as a readable path. */
export function codingAgentsLabel(slug) {
  const place = codingAgentsPlace(slug);
  return place ? place.path.join("/") : String(slug || "");
}

/**
 * A page URL the server built to its two halves, or null when it does not
 * name both. The server cites the web app's own page address,
 * `…/wiki?slug={slug}&scope={scope}` (`livingbrain_pages::page_url`); the
 * older `…/brain/{scope}/{slug}` form is still read, so a link copied before
 * the change keeps working. A `?slug=` with no scope names the slug alone
 * (scope ""), which is all the page route needs to open it. Relative URLs
 * are judged against the app's host.
 */
export function parseBrainUrl(url) {
  let parsed;
  try {
    parsed = new URL(String(url == null ? "" : url), "https://app.livingbrain.wiki");
  } catch {
    return null;
  }
  const querySlug = parsed.searchParams.get("slug");
  const queryScope = parsed.searchParams.get("scope");
  if (querySlug) return { scope: queryScope || "", slug: querySlug };
  const parts = parsed.pathname.split("/").filter(Boolean);
  const at = parts.indexOf("brain");
  if (at === -1 || parts.length - at < 3) return null;
  const scope = parts[at + 1];
  const slug = parts[at + 2];
  if (!scope || !slug) return null;
  return { scope, slug };
}

/**
 * Splits `value` around every occurrence of any term: text-node descriptions
 * between `mark` descriptions, longest terms first, case-insensitive. Terms
 * shorter than two characters are ignored, so a one-letter query never
 * shreds a paragraph. The output mounts through `mountNodes` like any other
 * node description — no HTML string is ever built.
 */
export function highlightTerms(value, terms) {
  const source = String(value == null ? "" : value);
  const wanted = [
    ...new Set(
      (Array.isArray(terms) ? terms : [])
        .map((term) => String(term).toLowerCase())
        .filter((term) => term.length >= 2),
    ),
  ].sort((a, b) => b.length - a.length);
  if (!source || !wanted.length) return source ? [tx(source)] : [];
  const lower = source.toLowerCase();
  const parts = [];
  let cursor = 0;
  while (cursor < source.length) {
    let best = -1;
    let bestTerm = "";
    for (const term of wanted) {
      const at = lower.indexOf(term, cursor);
      // `<` keeps the longest term when two match at the same index.
      if (at !== -1 && (best === -1 || at < best)) {
        best = at;
        bestTerm = term;
      }
    }
    if (best === -1) break;
    if (best > cursor) parts.push(tx(source.slice(cursor, best)));
    parts.push(el("mark", [tx(source.slice(best, best + bestTerm.length))]));
    cursor = best + bestTerm.length;
  }
  if (cursor < source.length) parts.push(tx(source.slice(cursor)));
  return parts;
}

/**
 * A wikilink body (the text between `[[` and `]]`) to its link, or null when
 * it must stay literal text. A bare `[[tamar-routes]]` names the address
 * itself, so it must already be a slug — lowercased first, so
 * `[[Tamar-Routes]]` links the same page. An aliased `[[orbit window|Orbit
 * Window]]` is free text naming a concept, so its target is slugified and the
 * alias becomes the label.
 */
export function wikilinkToLink(body) {
  const bar = String(body == null ? "" : body).indexOf("|");
  const target = (bar === -1 ? body : body.slice(0, bar)).trim();
  const alias = bar === -1 ? "" : body.slice(bar + 1).trim();
  const slug = alias ? slugify(target) : target.toLowerCase();
  if (!validSlug(slug)) return null;
  return { slug, label: alias || slug };
}

/**
 * The body after a leading frontmatter fence: when the very first line is
 * exactly `---`, everything up to the next line that is exactly `---` is
 * frontmatter, and what comes back is everything after that line's newline,
 * with at most one leading newline trimmed so the body starts clean. Without
 * a fence at the very start, or with no closing fence, the input comes back
 * unchanged. Display-only — the server's search snippets strip the same way,
 * but the editor's textarea and what Save sends keep the raw markdown.
 */
export function stripFrontmatter(markdown) {
  const source = String(markdown == null ? "" : markdown);
  const lines = source.split("\n");
  if (lines[0] !== "---") return source;
  for (let at = 1; at < lines.length; at += 1) {
    if (lines[at] !== "---") continue;
    let body = lines.slice(at + 1).join("\n");
    if (body.startsWith("\n")) body = body.slice(1);
    return body;
  }
  return source;
}

/* --------------------------------------------- tiny description builders */

function tx(value) {
  return { type: "text", value: String(value) };
}

function el(tag, children, attrs) {
  const node = { type: "element", tag, children: children || [] };
  if (attrs && Object.keys(attrs).length) node.attrs = attrs;
  return node;
}

/** The query's highlightable words: split on whitespace, two chars or more. */
function terms(query) {
  return String(query)
    .trim()
    .split(/\s+/)
    .filter((word) => word.length >= 2);
}

/** `highlightTerms`, but only the first match stays marked. */
function firstHighlight(value, list) {
  let seen = false;
  return highlightTerms(value, list).map((part) => {
    if (part.tag !== "mark" || seen) {
      return part.tag === "mark"
        ? tx(part.children.map((child) => child.value).join(""))
        : part;
    }
    seen = true;
    return part;
  });
}

/* ----------------------------------------------------------- the pins */

/** The stored pin keys, or [] when storage is empty, corrupt or refused. */
function readPins() {
  try {
    const value = JSON.parse(localStorage.getItem(PIN_KEY) || "[]");
    return Array.isArray(value) ? value.filter((v) => typeof v === "string") : [];
  } catch {
    return [];
  }
}

function writePins(pins) {
  try {
    localStorage.setItem(PIN_KEY, JSON.stringify(pins));
  } catch {
    /* storage can be refused; pins then last only this page view */
  }
}

/** A page's pin key. Scope rides in on list rows; on a page body it can only
 * be recovered from the `url` the server sends back, so that is where it is
 * read — the two agree because the server builds both. */
function pinId(row) {
  const where = parseBrainUrl(row && row.url);
  const scope = (row && row.scope) || (where && where.scope) || "";
  return `${scope}/${(row && row.slug) || ""}`;
}

/** A list row's `updated_at` as a locale date, "" when unparseable. */
function formatDate(value) {
  const date = new Date(value);
  if (!value || Number.isNaN(date.getTime())) return "";
  try {
    return date.toLocaleDateString(undefined, {
      month: "short",
      day: "numeric",
      year: "numeric",
    });
  } catch {
    return String(value);
  }
}

/* --------------------------------------------------------- the controller */

/**
 * The whole wiki page, one call per load. `api` is the app's fetch wrapper.
 * Everything the page does — session, routing, the four views, search, ask —
 * happens inside this closure, so a second call starts fresh.
 */
export async function wikiPage(api) {
  const params = new URLSearchParams(location.search);
  const startSlug = params.get("slug") || "";
  const startInEdit = params.get("mode") === "edit";
  const startView = params.get("view") || "";
  const startProject = validProject(params.get("project")) ? params.get("project") : "";

  const els = {
    views: {
      home: $("#home-view"),
      page: $("#page-view"),
      editor: $("#editor-view"),
      new: $("#new-page-view"),
      sessions: $("#sessions-view"),
      transcript: $("#transcript-view"),
      facts: $("#facts-view"),
    },
    notice: $("#wiki-notice"),
    status: $("#wiki-status"),
    homeStatus: $("#home-status"),
    listFilter: $("#list-filter"),
    pinnedSection: $("#pinned-section"),
    pinnedList: $("#pinned-list"),
    recentSection: $("#recent-section"),
    recentList: $("#recent-list"),
    homeNomatch: $("#home-nomatch"),
    homeEmpty: $("#home-empty"),
    pageTitle: $("#page-title"),
    pageMeta: $("#page-meta"),
    pageBody: $("#page-body"),
    pageBacklinks: $("#page-backlinks"),
    pageCitations: $("#page-citations"),
    pageFactsCard: $("#page-facts-card"),
    pageFacts: $("#page-facts"),
    pageFactsAsOf: $("#page-facts-as-of"),
    factsAsOf: $("#facts-as-of"),
    factsNow: $("#facts-now"),
    factsStatus: $("#facts-status"),
    factsList: $("#facts-list"),
    factsEmpty: $("#facts-empty"),
    tocDesktop: $("#toc-desktop"),
    tocList: $("#toc-list"),
    tocDrawerHeading: $("#toc-drawer-heading"),
    tocDrawer: $("#toc-drawer"),
    edit: $("#edit"),
    pinToggle: $("#pin-toggle"),
    askOpen: $("#ask-open"),
    askOpenEdit: $("#ask-open-edit"),
    editTitle: $("#edit-title"),
    editWrite: $("#edit-write"),
    editPreview: $("#edit-preview"),
    source: $("#source"),
    preview: $("#preview"),
    save: $("#save"),
    cancelEdit: $("#cancel-edit"),
    npForm: $("#np-form"),
    npSlug: $("#np-slug"),
    npSlugError: $("#np-slug-error"),
    npTitle: $("#np-title"),
    npCreate: $("#np-create"),
    npCancel: $("#np-cancel"),
    searchForm: $("#wiki-search-form"),
    searchInput: $("#wiki-search"),
    searchResults: $("#wiki-search-results"),
    searchProject: $("#wiki-project"),
    sessionsProject: $("#sessions-project"),
    sessionsStatus: $("#sessions-status"),
    sessionsList: $("#sessions-list"),
    sessionsEmpty: $("#sessions-empty"),
    sessionsMore: $("#sessions-more"),
    caCard: $("#ca-card"),
    caSummary: $("#ca-summary"),
    caModel: $("#ca-model"),
    caProcess: $("#ca-process"),
    caResult: $("#ca-result"),
    caSection: $("#coding-agents-section"),
    caTree: $("#coding-agents-tree"),
    transcriptCompile: $("#transcript-compile"),
    transcriptTitle: $("#transcript-title"),
    transcriptStatus: $("#transcript-status"),
    transcriptBody: $("#transcript-body"),
    drawerToggle: $("#drawer-toggle"),
    drawer: $("#wiki-drawer"),
    drawerSignin: $("#drawer-signin"),
    drawerSignout: $("#drawer-signout"),
    askPanel: $("#ask-panel"),
    askQuestion: $("#ask-question"),
    askSubmit: $("#ask-submit"),
    askClose: $("#ask-close"),
    askStatus: $("#ask-status"),
    askAnswer: $("#ask-answer"),
    askCitations: $("#ask-citations"),
  };

  /** The page's state. One load of `wikiPage` owns one of these. */
  const state = {
    signedIn: false,
    session: null,
    slug: startSlug,
    page: null,
    version: null,
    rows: [],
    filter: "",
    searchResults: [],
    searchOptions: [],
    activeSearch: -1,
    previewing: false,
    // The header search's project filter: "" is every project.
    project: "",
    // The projects the asker's sources are filed under (`/v1/sources/projects`).
    projects: [],
    sessions: [],
    sessionsProject: startProject,
    sessionsCursor: null,
    // The compile queue, keyed by source id (`/v1/coding-agents/queue`).
    jobs: new Map(),
  };
  let searchTimer = null;

  /* -- small shared pieces ------------------------------------------ */

  const VIEWS = ["home", "page", "editor", "new", "sessions", "transcript", "facts"];

  function showView(name) {
    for (const view of VIEWS) {
      if (els.views[view]) els.views[view].hidden = view !== name;
    }
  }

  /** A plain button with a handler, for the notice area's actions. */
  function mkButton(label, handler) {
    const button = document.createElement("button");
    button.type = "button";
    button.textContent = label;
    button.addEventListener("click", handler);
    return button;
  }

  /** The signed-out message: text plus the link to the sign-in page. */
  function signInNotice(message) {
    notice(els.notice, message, "warn");
    const link = document.createElement("a");
    link.href = "index.html";
    link.textContent = "Sign in";
    els.notice.appendChild(link);
  }

  function clearNotice() {
    notice(els.notice, "", "ok");
  }

  /* -- header session bar + drawer mirrors -------------------------- */

  function paintSession() {
    applyHeader(
      {
        bar: $("#session-bar"),
        who: $("#session-who"),
        signin: $("#header-signin"),
      },
      state.session,
    );
    if (els.drawerSignin) els.drawerSignin.hidden = state.signedIn;
    if (els.drawerSignout) els.drawerSignout.hidden = !state.signedIn;
    // Ask hits the API like Edit and Pin do, so its two buttons stand down
    // with the write affordances wherever they live (page toolbar, editor).
    for (const button of [els.edit, els.pinToggle, els.askOpen, els.askOpenEdit]) {
      if (button) button.hidden = !state.signedIn;
    }
    paintNewPage();
  }

  /** The New page affordances are write affordances, exactly like Edit: they
   * hide signed out, wherever they appear — the home toolbar, the empty
   * state and the page-view toolbar. */
  function paintNewPage() {
    for (const trigger of $$("[data-new-page]")) trigger.hidden = !state.signedIn;
  }

  function openDrawer() {
    els.drawer.hidden = false;
    els.drawerToggle.setAttribute("aria-expanded", "true");
    const first = els.drawer.querySelector("a");
    if (first && typeof first.focus === "function") first.focus();
  }

  function closeDrawer() {
    els.drawer.hidden = true;
    els.drawerToggle.setAttribute("aria-expanded", "false");
  }

  /* -- home ---------------------------------------------------------- */

  function rowElement(row, pinnable) {
    const li = document.createElement("li");
    li.className = "wiki-row";
    const link = document.createElement("a");
    link.className = "wiki-row__link";
    link.href = `wiki.html?slug=${encodeURIComponent(row.slug || "")}`;
    const slug = document.createElement("span");
    slug.className = "wiki-row__slug";
    slug.textContent = row.slug || "(unnamed)";
    const pill = document.createElement("span");
    pill.className = "pill";
    pill.textContent = row.entity_type || "page";
    const meta = document.createElement("span");
    meta.className = "wiki-row__meta";
    meta.textContent = [formatDate(row.updated_at), row.version != null ? `v${row.version}` : ""]
      .filter(Boolean)
      .join(" · ");
    link.append(slug, pill, meta);
    li.appendChild(link);
    if (pinnable) {
      const unpin = mkButton("Unpin", () => {
        writePins(readPins().filter((id) => id !== pinId(row)));
        renderHome();
      });
      unpin.className = "wiki-row__pin";
      unpin.setAttribute("aria-label", `Unpin ${row.slug || "page"}`);
      li.appendChild(unpin);
    }
    return li;
  }

  function fillList(list, rows, pinnable) {
    list.replaceChildren();
    for (const row of rows) list.appendChild(rowElement(row, pinnable));
  }

  function renderHome() {
    const query = state.filter.trim().toLowerCase();
    const matches = state.rows.filter(
      (row) =>
        !query ||
        `${row.slug || ""} ${row.entity_type || ""}`.toLowerCase().includes(query),
    );
    const pins = readPins();
    const pinned = matches.filter((row) => pins.includes(pinId(row)));
    // The compiled coding-agents pages are a folder of their own, not
    // recent pages: they would crowd everything else off the list.
    const recent = matches.filter(
      (row) => !pins.includes(pinId(row)) && !codingAgentsPlace(row.slug),
    );
    fillList(els.pinnedList, pinned, true);
    fillList(els.recentList, recent, false);
    els.pinnedSection.hidden = pinned.length === 0;
    els.recentSection.hidden = recent.length === 0;
    renderCodingAgents(matches, query !== "");
    els.listFilter.hidden = state.rows.length === 0;
    els.homeEmpty.hidden = state.rows.length !== 0;
    els.homeNomatch.hidden = !(state.rows.length > 0 && matches.length === 0);
  }

  /** One page link in the coding-agents tree, labelled by its last segment. */
  function treeLink(entry) {
    const li = document.createElement("li");
    const link = document.createElement("a");
    link.href = `wiki.html?slug=${encodeURIComponent(entry.row.slug || "")}`;
    link.textContent = entry.place.kind === "rollup" ? entry.place.project : entry.place.page;
    li.appendChild(link);
    return li;
  }

  /** A folder: `<details>` with its name and its pages and subfolders. */
  function treeFolder(name, entries, children, open) {
    const details = document.createElement("details");
    details.className = "ca-folder";
    if (open) details.setAttribute("open", "");
    const summary = document.createElement("summary");
    summary.textContent = `${name}/`;
    details.appendChild(summary);
    const list = document.createElement("ul");
    list.className = "ca-folder__list";
    for (const entry of entries) list.appendChild(treeLink(entry));
    for (const child of children) {
      const li = document.createElement("li");
      li.appendChild(child);
      list.appendChild(li);
    }
    details.appendChild(list);
    return details;
  }

  /** The `coding-agents/` folder on the home view: agent, then project. */
  function renderCodingAgents(rows, filtering) {
    if (!els.caSection || !els.caTree) return;
    const tree = codingAgentsTree(rows);
    const empty = !tree.top.length && !tree.agents.length && !tree.rollups.length;
    els.caSection.hidden = empty;
    els.caTree.replaceChildren();
    if (empty) return;
    const agents = tree.agents.map((agent) =>
      treeFolder(
        agent.agent,
        agent.pages,
        agent.projects.map((project) => treeFolder(project.project, project.pages, [], filtering)),
        filtering,
      ),
    );
    const folders = [...agents];
    if (tree.rollups.length) folders.push(treeFolder(ROLLUP, tree.rollups, [], filtering));
    els.caTree.appendChild(treeFolder(CODING_AGENTS, tree.top, folders, true));
  }

  function renderHomeSignedOut() {
    if (els.caSection) els.caSection.hidden = true;
    els.listFilter.hidden = true;
    els.pinnedSection.hidden = true;
    els.recentSection.hidden = true;
    els.homeNomatch.hidden = true;
    els.homeEmpty.hidden = true;
    els.homeStatus.textContent = "";
    signInNotice("Sign in to see the wiki.");
  }

  async function loadHome() {
    showView("home");
    if (!state.signedIn) {
      renderHomeSignedOut();
      return;
    }
    try {
      const data = await api("/v1/pages?limit=200");
      state.rows = data && Array.isArray(data.pages) ? data.pages : [];
      els.homeStatus.textContent = `${state.rows.length} ${
        state.rows.length === 1 ? "page" : "pages"
      }`;
      renderHome();
      clearNotice();
    } catch (error) {
      if (error instanceof ApiError && error.status === 401) {
        state.signedIn = false;
        paintSession();
        renderHomeSignedOut();
        return;
      }
      renderHomeSignedOut();
      notice(els.notice, `Could not load pages: ${error.message}`, "bad");
      els.notice.appendChild(mkButton("Retry", loadHome));
    }
  }

  /* -- projects: the filter both the search and the sessions list use -- */

  /** Fills one `<select>` with "All projects" and then every project, keeps
   * `selected` chosen when it is still one of them, and hides the control
   * when there is nothing to choose between. */
  function fillProjectSelect(select, selected) {
    if (!select) return;
    select.replaceChildren();
    const all = document.createElement("option");
    all.value = "";
    all.textContent = "All projects";
    select.appendChild(all);
    for (const entry of state.projects) {
      const option = document.createElement("option");
      option.value = entry.project;
      option.textContent = `${entry.project} (${entry.sources})`;
      select.appendChild(option);
    }
    const known = state.projects.some((entry) => entry.project === selected);
    select.value = known ? selected : "";
    select.hidden = state.projects.length === 0;
  }

  /** The projects the asker's sources are filed under. A failure here costs
   * the filter, never the page: the selects simply stay hidden. */
  async function loadProjects() {
    try {
      const data = await api("/v1/sources/projects");
      const list = data && Array.isArray(data.projects) ? data.projects : [];
      state.projects = list.filter((entry) => entry && validProject(entry.project));
    } catch {
      state.projects = [];
    }
    fillProjectSelect(els.searchProject, state.project);
    fillProjectSelect(els.sessionsProject, state.sessionsProject);
  }

  /* -- agent sessions ------------------------------------------------ */

  function sessionElement(row) {
    const li = document.createElement("li");
    li.className = "wiki-row";
    // Each session opens its transcript; the address names it, so a reload
    // or a shared link lands on the same one.
    const line = document.createElement("a");
    line.className = "wiki-row__link";
    line.href = transcriptHref(row.id || "");
    const name = document.createElement("span");
    name.className = "wiki-row__slug";
    name.textContent = row.path || row.id || "(unnamed session)";
    line.appendChild(name);
    if (row.project) {
      const pill = document.createElement("span");
      pill.className = "pill";
      pill.textContent = row.project;
      line.appendChild(pill);
    }
    const job = state.jobs.get(row.id);
    if (job) {
      const status = document.createElement("span");
      status.className = `pill ca-status ca-status--${job.status}`;
      status.textContent = jobLabel(job.status);
      if (job.error) status.setAttribute("title", job.error);
      line.appendChild(status);
    }
    const meta = document.createElement("span");
    meta.className = "wiki-row__meta";
    meta.textContent = [formatDate(row.created_at), row.scope === "shared" ? "shared" : ""]
      .filter(Boolean)
      .join(" · ");
    line.appendChild(meta);
    li.appendChild(line);
    if (job) {
      const pages = compiledPages(job);
      if (pages) li.appendChild(pages);
    }
    return li;
  }

  /** The wiki pages one compile wrote, as links; null when it wrote none. */
  function compiledPages(job) {
    const slugs = (Array.isArray(job.pages) ? job.pages : []).filter(
      (slug) => codingAgentsPlace(slug) && slug !== `${CODING_AGENTS}--log`,
    );
    if (!slugs.length) return null;
    const wrap = document.createElement("span");
    wrap.className = "ca-pages";
    wrap.appendChild(document.createTextNode("Wrote "));
    slugs.forEach((slug, index) => {
      if (index) wrap.appendChild(document.createTextNode(", "));
      const link = document.createElement("a");
      link.href = `wiki.html?slug=${encodeURIComponent(slug)}`;
      link.textContent = codingAgentsLabel(slug).replace(`${CODING_AGENTS}/`, "");
      wrap.appendChild(link);
    });
    return wrap;
  }

  /** The queue card: counts, the model, the budget, and Process now. */
  function paintQueueCard(data) {
    if (!els.caCard) return;
    if (!data) {
      els.caCard.hidden = true;
      return;
    }
    els.caCard.hidden = false;
    els.caSummary.textContent = queueSummary(data.counts);
    els.caModel.replaceChildren();
    const model = data.model || {};
    if (model.connected) {
      els.caModel.textContent = `Compiling with ${model.provider || "your model"} · ${
        model.model || ""
      } (${model.role || "main"}).`;
    } else {
      els.caModel.textContent =
        "No model connected, so nothing is compiled yet. Connect one under ";
      const link = document.createElement("a");
      link.href = "settings.html";
      link.textContent = "Settings → Models";
      els.caModel.appendChild(link);
      els.caModel.appendChild(document.createTextNode(" (Main)."));
    }
    const budget = data.budget || {};
    if (budget.daily_tokens != null) {
      els.caModel.appendChild(
        document.createTextNode(
          ` Budget today: ${budget.spent_today || 0} of ${budget.daily_tokens} tokens.`,
        ),
      );
    }
  }

  /** The compile queue for the sessions list. A failure hides the card and
   * the statuses, never the list. */
  async function loadQueue() {
    try {
      const data = await api(queuePath());
      state.jobs = jobsBySource(data && data.jobs);
      paintQueueCard(data);
    } catch {
      state.jobs = new Map();
      paintQueueCard(null);
    }
  }

  /** `Process now`: compile the next of the asker's sessions in the
   * request, then show the result and refresh the list. */
  async function processNow() {
    if (!els.caProcess) return;
    els.caProcess.disabled = true;
    els.caResult.textContent = "Compiling…";
    try {
      const data = await api("/v1/coding-agents/process", { method: "POST", body: { max: 1 } });
      const done = data && Array.isArray(data.processed) ? data.processed : [];
      els.caResult.textContent = done.length
        ? done.map((job) => jobLabel(job.status)).join(", ")
        : "Nothing waiting.";
    } catch (error) {
      els.caResult.textContent = `Could not compile: ${error.message}`;
    } finally {
      els.caProcess.disabled = false;
    }
    await loadQueue();
    renderSessions();
  }

  function renderSessions() {
    const rows = latestSessions(state.sessions);
    els.sessionsList.replaceChildren();
    for (const row of rows) els.sessionsList.appendChild(sessionElement(row));
    els.sessionsList.hidden = rows.length === 0;
    els.sessionsEmpty.hidden = rows.length !== 0;
    els.sessionsMore.hidden = !state.sessionsCursor;
    const count = rows.length;
    els.sessionsStatus.textContent = `${count}${state.sessionsCursor ? "+" : ""} ${
      count === 1 ? "session" : "sessions"
    }`;
  }

  /** One page of sessions: the first (replacing the list) or the next one
   * (`more`, appended after the cursor). */
  async function loadSessions({ more = false } = {}) {
    showView("sessions");
    if (!state.signedIn) {
      if (els.caCard) els.caCard.hidden = true;
      els.sessionsList.hidden = true;
      els.sessionsEmpty.hidden = true;
      els.sessionsMore.hidden = true;
      if (els.sessionsProject) els.sessionsProject.hidden = true;
      els.sessionsStatus.textContent = "";
      signInNotice("Sign in to see your agent sessions.");
      return;
    }
    const cursor = more ? state.sessionsCursor : "";
    try {
      const data = await api(sessionsPath({ project: state.sessionsProject, cursor }));
      const rows = data && Array.isArray(data.sources) ? data.sources : [];
      state.sessions = more ? state.sessions.concat(rows) : rows;
      state.sessionsCursor = (data && data.next_cursor) || null;
      if (!more) await loadQueue();
      renderSessions();
      clearNotice();
    } catch (error) {
      if (error instanceof ApiError && error.status === 401) {
        state.signedIn = false;
        paintSession();
        await loadSessions();
        return;
      }
      notice(els.notice, `Could not load agent sessions: ${error.message}`, "bad");
      els.notice.appendChild(mkButton("Retry", () => loadSessions({ more })));
    }
  }

  /** What the compiler made of this session, above its transcript. */
  async function paintTranscriptCompile(id) {
    if (!els.transcriptCompile) return;
    els.transcriptCompile.replaceChildren();
    els.transcriptCompile.hidden = true;
    try {
      const data = await api(queuePath(id));
      const job = jobsBySource(data && data.jobs).get(id);
      if (!job) return;
      els.transcriptCompile.appendChild(
        document.createTextNode(`Wiki: ${jobLabel(job.status)}${job.error ? ` (${job.error})` : ""}. `),
      );
      const pages = compiledPages(job);
      if (pages) els.transcriptCompile.appendChild(pages);
      els.transcriptCompile.hidden = false;
    } catch {
      // The status is a nicety: the transcript stands without it.
    }
  }

  /** One session's transcript, read from its source: the record parsed
   * and drawn turn by turn. A body that is not a session record is shown
   * as the text it is. */
  async function loadTranscript(id) {
    showView("transcript");
    els.transcriptBody.replaceChildren();
    els.transcriptTitle.textContent = "Agent session";
    if (!state.signedIn) {
      signInNotice("Sign in to see your agent sessions.");
      return;
    }
    els.transcriptStatus.textContent = "Loading…";
    try {
      const source = await api(sourcePath(id));
      els.transcriptTitle.textContent =
        (source && (source.project ? `${source.project} · ` : "")) +
        (source && source.kind === "agent_log" ? "agent session" : "source");
      const record = source && source.kind === "agent_log" ? parseSessionRecord(source.body) : null;
      if (record) {
        els.transcriptTitle.textContent = `${record.agent || "agent"} session ${String(
          record.session_id || "",
        ).slice(0, 8)}${source.project ? ` · ${source.project}` : ""}`;
        renderInto(els.transcriptBody, transcriptNodes(record));
        await paintTranscriptCompile(id);
      } else {
        renderInto(els.transcriptBody, [
          el("pre", [el("code", [tx((source && source.body) || "")])]),
        ]);
      }
      els.transcriptStatus.textContent = "";
      clearNotice();
    } catch (error) {
      els.transcriptStatus.textContent = "";
      if (error instanceof ApiError && error.status === 401) {
        state.signedIn = false;
        paintSession();
        signInNotice("Sign in to see your agent sessions.");
        return;
      }
      notice(
        els.notice,
        error instanceof ApiError && error.status === 404
          ? "That session is not one you can open."
          : `Could not load the session: ${error.message}`,
        "bad",
      );
    }
  }

  /* -- page view ----------------------------------------------------- */

  function currentPinId() {
    return state.page ? pinId(state.page) : "";
  }

  function paintPin() {
    const pinned = readPins().includes(currentPinId());
    els.pinToggle.textContent = pinned ? "Unpin" : "Pin";
    els.pinToggle.setAttribute("aria-pressed", String(pinned));
  }

  function togglePin() {
    const id = currentPinId();
    if (!id || id.endsWith("/")) return;
    const pins = readPins();
    const at = pins.indexOf(id);
    if (at === -1) pins.unshift(id);
    else pins.splice(at, 1);
    writePins(pins);
    paintPin();
  }

  /** Heading ids (slugged, deduped `-2`, `-3`…), then a TOC of the h2/h3s
   * into both mounts — the desktop aside and the drawer. */
  function wireHeadingsAndToc() {
    const used = new Map();
    const entries = [];
    for (const heading of $$("h1, h2, h3, h4, h5, h6", els.pageBody)) {
      const base = slugify(heading.textContent) || "section";
      const seen = used.get(base) || 0;
      used.set(base, seen + 1);
      const id = seen ? `${base}-${seen + 1}` : base;
      heading.setAttribute("id", id);
      entries.push({
        id,
        level: String(heading.tagName || heading.name || "h6").toLowerCase(),
        text: heading.textContent,
      });
    }
    const toc = entries.filter((e) => e.level === "h2" || e.level === "h3");
    const description = el(
      "ul",
      toc.map((entry) =>
        el(
          "li",
          [
            el("a", [tx(entry.text)], {
              href: `#${entry.id}`,
            }),
          ],
          entry.level === "h3" ? { class: "wiki-toc__sub" } : null,
        ),
      ),
    );
    renderInto(els.tocList, description);
    renderInto(els.tocDrawer, description);
    const enough = toc.length >= 2;
    els.tocDesktop.hidden = !enough;
    if (els.tocDrawerHeading) els.tocDrawerHeading.hidden = !enough;
  }

  /** The backlinks and citations cards: each is a plain `section.card` of
   * heading, blurb and one list, so a card whose list came back empty — a
   * legitimately linkless page, an error, a signed-out visit — is all heading
   * and no body. The whole card stands down then, signed in or not. */
  function paintCards(backlinks, citations) {
    for (const [content, empty] of [
      [els.pageBacklinks, backlinks.length === 0],
      [els.pageCitations, citations.length === 0],
    ]) {
      content.hidden = empty;
      const card = content.parentNode;
      if (card) card.hidden = empty;
    }
  }

  function clearArticle() {
    renderInto(els.pageBody, []);
    renderInto(els.pageBacklinks, []);
    renderInto(els.pageCitations, []);
    els.pageMeta.replaceChildren();
    paintCards([], []);
    paintPageFacts([]);
    wireHeadingsAndToc();
  }

  /** The signed-out page shell: the title (the slug) and the sign-in notice,
   * nothing else — no article, no meta, no toolbar, no empty cards. */
  function paintSignedOutPage() {
    els.pageBody.hidden = true;
    els.pageMeta.hidden = true;
    clearArticle();
    signInNotice("Sign in to read this page.");
  }

  /**
   * Replaces every `[[slug]]` / `[[slug|Label]]` in the article's text with a
   * real anchor: a `TreeWalker` over the text nodes, each swapped in place
   * for text nodes and anchors built with `createElement` + `textContent`,
   * like everything else that reaches the document. A body that never yields
   * a valid slug — `[[orbit window]]` bare, say — keeps the literal brackets
   * it was written with, and `pre`/`code` are never touched: there `[[…]]` is
   * quoted text, not a link.
   */
  function linkWikilinks(container) {
    if (!container || typeof document.createTreeWalker !== "function") return;
    const walker = document.createTreeWalker(container, SHOW_TEXT);
    const nodes = [];
    while (walker.nextNode()) nodes.push(walker.currentNode);
    const pattern = /\[\[([^[\]]+)\]\]/g;
    for (const node of nodes) {
      const owner = node.parentNode;
      const value = String(
        node.nodeValue != null ? node.nodeValue : node.textContent || "",
      );
      if (!owner || !value.includes("[[")) continue;
      if (typeof owner.closest === "function" && owner.closest("pre, code")) {
        continue;
      }
      const parts = [];
      let cursor = 0;
      let match;
      while ((match = pattern.exec(value)) !== null) {
        const link = wikilinkToLink(match[1]);
        if (!link) continue;
        if (match.index > cursor) {
          parts.push(document.createTextNode(value.slice(cursor, match.index)));
        }
        const anchor = document.createElement("a");
        anchor.setAttribute(
          "href",
          `wiki.html?slug=${encodeURIComponent(link.slug)}`,
        );
        anchor.textContent = link.label;
        parts.push(anchor);
        cursor = match.index + match[0].length;
      }
      if (!parts.length) continue;
      if (cursor < value.length) {
        parts.push(document.createTextNode(value.slice(cursor)));
      }
      node.replaceWith(...parts);
    }
  }

  function renderPage() {
    const page = state.page || {};
    els.pageBody.hidden = false;
    els.pageMeta.hidden = false;
    els.pageTitle.textContent = page.title || page.slug || "Untitled";
    const pill = document.createElement("span");
    pill.className = "pill";
    pill.textContent = page.entity_type || "page";
    els.pageMeta.replaceChildren();
    els.pageMeta.appendChild(pill);
    const where = parseBrainUrl(page.url);
    const facts = [];
    if (page.version != null) facts.push(`v${page.version}`);
    if (where && where.scope) facts.push(where.scope);
    if (codingAgentsPlace(page.slug)) facts.push(codingAgentsLabel(page.slug));
    if (facts.length) els.pageMeta.appendChild(document.createTextNode(facts.join(" · ")));
    renderInto(els.pageBody, renderMarkdown(stripFrontmatter(page.markdown || "")));
    linkWikilinks(els.pageBody);
    wireHeadingsAndToc();
    const backlinks = Array.isArray(page.backlinks) ? page.backlinks : [];
    const citations = Array.isArray(page.citations) ? page.citations : [];
    renderInto(els.pageBacklinks, renderBacklinks(backlinks));
    renderInto(els.pageCitations, renderCitations(citations));
    paintCards(backlinks, citations);
    if (els.pageFactsAsOf) els.pageFactsAsOf.value = "";
    paintPageFacts(page.facts);
    paintPin();
  }

  /* -- temporal facts (docs/temporal-facts.md) ------------------------ */

  /** The page's Facts card: Current then History, or no card at all when the
   * entity has no facts the reader may see. */
  function paintPageFacts(keys) {
    const description = factsDescription(keys);
    renderInto(els.pageFacts, description);
    els.pageFactsCard.hidden = description.length === 0;
  }

  /** Re-reads the open page's facts as of the picked date ("" = now). */
  async function pageFactsAsOf(asOf) {
    const page = state.page;
    if (!page || !page.subject) return;
    try {
      const data = await api(factsPath({ subject: page.subject, asOf }));
      paintPageFacts(data && data.keys);
    } catch (error) {
      if (error instanceof ApiError && error.status === 401) return;
      notice(els.notice, `Could not load facts: ${error.message}`, "bad");
    }
  }

  /** The Facts view: every timeline the reader may see, as of a date. */
  async function loadFacts(asOf = "") {
    showView("facts");
    if (!state.signedIn) {
      renderInto(els.factsList, []);
      els.factsList.hidden = true;
      els.factsEmpty.hidden = true;
      els.factsStatus.textContent = "";
      signInNotice("Sign in to see the facts you can read.");
      return;
    }
    els.factsStatus.textContent = "Loading…";
    try {
      const data = await api(factsPath({ asOf }));
      const keys = (data && data.keys) || [];
      const description = factsDescription(keys, { showSubject: true });
      renderInto(els.factsList, description);
      els.factsList.hidden = description.length === 0;
      els.factsEmpty.hidden = description.length !== 0;
      els.factsStatus.textContent = asOf ? `As of ${asOf}` : "Now";
    } catch (error) {
      els.factsStatus.textContent = "";
      if (error instanceof ApiError && error.status === 401) {
        signInNotice("Sign in to see the facts you can read.");
        return;
      }
      notice(els.notice, `Could not load facts: ${error.message}`, "bad");
    }
  }

  async function openPage(targetSlug, { edit = false } = {}) {
    state.slug = targetSlug;
    state.page = null;
    state.version = null;
    showView("page");
    if (!state.signedIn) {
      els.pageTitle.textContent = targetSlug;
      paintSignedOutPage();
      return;
    }
    let page;
    try {
      page = await api(`/v1/pages/${encodeURIComponent(targetSlug)}`);
    } catch (error) {
      els.pageTitle.textContent = targetSlug;
      els.pageBody.hidden = true;
      els.pageMeta.hidden = true;
      clearArticle();
      els.edit.hidden = true;
      els.pinToggle.hidden = true;
      els.askOpen.hidden = true;
      if (error instanceof ApiError && error.status === 404) {
        notice(
          els.notice,
          "No page has this slug yet — the New page button creates it.",
          "warn",
        );
        return;
      }
      if (error instanceof ApiError && error.status === 401) {
        state.signedIn = false;
        paintSession();
        paintSignedOutPage();
        return;
      }
      notice(els.notice, `Could not open ${targetSlug}: ${error.message}`, "bad");
      return;
    }
    state.page = page;
    state.version = page.version == null ? null : page.version;
    if (edit) openEditor();
    else {
      renderPage();
      clearNotice();
    }
  }

  /* -- editor ---------------------------------------------------------- */

  function paintEditMode() {
    els.source.hidden = state.previewing;
    els.preview.hidden = !state.previewing;
    els.editWrite.setAttribute("aria-pressed", String(!state.previewing));
    els.editPreview.setAttribute("aria-pressed", String(state.previewing));
  }

  /** The preview pane: always the textarea's current content, but stripped of
   * its frontmatter fence — a display, like the read view, never the source. */
  function renderPreview() {
    renderInto(els.preview, renderMarkdown(stripFrontmatter(els.source.value)));
  }

  function openEditor() {
    const page = state.page || {};
    els.editTitle.value = page.title || "";
    els.source.value = page.markdown || "";
    state.previewing = false;
    renderPreview();
    paintEditMode();
    els.status.textContent = "";
    clearNotice();
    showView("editor");
    if (typeof els.editTitle.focus === "function") els.editTitle.focus();
  }

  /** Lands the editor's result back on the read view, from the server's
   * response — the store redacts secrets before saving, so what it returns,
   * not what was sent, is the page's truth. */
  function applySave(saved, sent) {
    state.page = saved;
    if (saved && saved.version != null) state.version = saved.version;
    if (saved && saved.slug) state.slug = saved.slug;
    if (saved && saved.markdown != null) els.source.value = saved.markdown;
    if (state.slug) {
      history.replaceState(
        null,
        "",
        `${location.pathname}?slug=${encodeURIComponent(state.slug)}`,
      );
    }
    renderPage();
    showView("page");
    const redacted = Boolean(saved && saved.markdown != null && saved.markdown !== sent);
    notice(
      els.notice,
      redacted ? "Sensitive-looking text was redacted before saving." : "Saved.",
      redacted ? "warn" : "ok",
    );
  }

  async function savePage() {
    if (!state.slug) return;
    const sent = els.source.value;
    els.save.disabled = true;
    try {
      const saved = await api(`/v1/pages/${encodeURIComponent(state.slug)}`, {
        method: "PUT",
        body: {
          markdown: sent,
          base_version: state.version,
          title: els.editTitle.value.trim(),
        },
      });
      applySave(saved, sent);
    } catch (error) {
      if (error instanceof ApiError && error.status === 409) showConflict();
      else notice(els.notice, `Not saved: ${error.message}`, "bad");
    } finally {
      els.save.disabled = false;
    }
  }

  /** The optimistic-concurrency refusal: keep the draft, offer the two ways
   * out as buttons on the notice itself. */
  function showConflict() {
    notice(
      els.notice,
      "Someone else saved this page while you were editing. Your text is " +
        "still here — load their version, or overwrite it with yours.",
      "bad",
    );
    els.notice.appendChild(mkButton("Load their version", loadTheirs));
    els.notice.appendChild(mkButton("Overwrite with mine", overwriteMine));
  }

  async function loadTheirs() {
    try {
      const fresh = await api(`/v1/pages/${encodeURIComponent(state.slug)}`);
      state.page = fresh;
      state.version = fresh.version == null ? null : fresh.version;
      els.source.value = fresh.markdown || "";
      els.editTitle.value = fresh.title || "";
      renderPreview();
      notice(els.notice, "Loaded the current version — your draft was replaced.", "warn");
    } catch (error) {
      notice(els.notice, `Could not load the current version: ${error.message}`, "bad");
    }
  }

  async function overwriteMine() {
    const sent = els.source.value;
    try {
      const fresh = await api(`/v1/pages/${encodeURIComponent(state.slug)}`);
      const saved = await api(`/v1/pages/${encodeURIComponent(state.slug)}`, {
        method: "PUT",
        body: {
          markdown: sent,
          base_version: fresh.version,
          title: els.editTitle.value.trim(),
        },
      });
      applySave(saved, sent);
    } catch (error) {
      notice(els.notice, `Not saved: ${error.message}`, "bad");
    }
  }

  /** Back out of the editor or the new-page form to wherever the page was. */
  function leaveEditing() {
    clearNotice();
    if (state.page) {
      renderPage();
      showView("page");
    } else {
      loadHome();
    }
  }

  /* -- new page -------------------------------------------------------- */

  function paintNpValidity() {
    const value = els.npSlug.value;
    const ok = validSlug(value);
    els.npCreate.disabled = !ok;
    els.npSlugError.textContent = ok ? "" : SLUG_MESSAGE;
    // `aria-invalid` is also the cue wiki.css uses to turn the slot
    // danger-red, so it goes on only once validation has actually failed: a
    // slug was typed and refused. A pristine (blank) field keeps the rule as
    // neutral helper text — the disabled Create button already says the form
    // is not ready.
    if (ok || value === "") els.npSlug.removeAttribute("aria-invalid");
    else els.npSlug.setAttribute("aria-invalid", "true");
  }

  function openNewPage() {
    els.npSlug.value = "";
    els.npTitle.value = "";
    paintNpValidity();
    showView("new");
    clearNotice();
    if (typeof els.npSlug.focus === "function") els.npSlug.focus();
  }

  async function createPage(event) {
    if (event && typeof event.preventDefault === "function") event.preventDefault();
    const slug = els.npSlug.value;
    if (!validSlug(slug)) {
      paintNpValidity();
      if (typeof els.npSlug.focus === "function") els.npSlug.focus();
      return;
    }
    els.npCreate.disabled = true;
    try {
      const saved = await api(`/v1/pages/${encodeURIComponent(slug)}`, {
        method: "PUT",
        body: { markdown: "", base_version: null, title: els.npTitle.value.trim() },
      });
      state.page = saved;
      state.version = saved && saved.version != null ? saved.version : null;
      state.slug = (saved && saved.slug) || slug;
      history.replaceState(
        null,
        "",
        `${location.pathname}?slug=${encodeURIComponent(state.slug)}`,
      );
      renderPage();
      showView("page");
      notice(els.notice, "Created.", "ok");
    } catch (error) {
      if (error instanceof ApiError && error.status === 401) {
        state.signedIn = false;
        paintSession();
        signInNotice("Sign in to create a page.");
      } else {
        els.npSlugError.textContent = error.message;
        els.npSlug.setAttribute("aria-invalid", "true");
      }
    } finally {
      // Re-arm the button from the slug's validity alone — a full repaint
      // here would wipe a server error just written under the field.
      els.npCreate.disabled = !validSlug(els.npSlug.value);
    }
  }

  /* -- ask --------------------------------------------------------------- */

  function openAsk() {
    const page = state.page || {};
    const subject = page.title || page.slug || "the wiki";
    els.askQuestion.value = `What is "${subject}" about?`;
    els.askStatus.textContent = "";
    renderInto(els.askAnswer, []);
    renderInto(els.askCitations, []);
    if (typeof els.askPanel.showModal === "function") els.askPanel.showModal();
    else els.askPanel.hidden = false;
    if (typeof els.askQuestion.focus === "function") els.askQuestion.focus();
  }

  function closeAsk() {
    if (typeof els.askPanel.close === "function") els.askPanel.close();
    else els.askPanel.hidden = true;
  }

  /** Ask citations: the title as a link when its URL is safe, the quoted line
   * under it as a blockquote. */
  function askCitations(list) {
    const items = (Array.isArray(list) ? list : []).map((raw) => {
      const citation = raw || {};
      const title = String(citation.title == null ? "Untitled source" : citation.title);
      const url = String(citation.url == null ? "" : citation.url).trim();
      const quote = String(citation.quote == null ? "" : citation.quote).trim();
      const children = [
        isSafeHref(url)
          ? el("a", [tx(title)], { href: url, rel: "noopener noreferrer" })
          : el("span", [tx(title)]),
      ];
      if (quote) children.push(el("blockquote", [tx(quote)]));
      return el("li", children);
    });
    return el("ul", items, { class: "citations" });
  }

  async function submitAsk() {
    const question = els.askQuestion.value.trim();
    if (!question) {
      els.askStatus.textContent = "Type a question first.";
      return;
    }
    const label = els.askSubmit.textContent;
    els.askSubmit.disabled = true;
    els.askSubmit.textContent = "Asking…";
    els.askStatus.textContent = "";
    try {
      const answer = await api("/v1/ask", { method: "POST", body: { question } });
      renderInto(els.askAnswer, renderMarkdown((answer && answer.answer) || ""));
      renderInto(els.askCitations, askCitations(answer && answer.citations));
    } catch (error) {
      els.askStatus.textContent = `Could not ask: ${error.message}`;
    } finally {
      els.askSubmit.disabled = false;
      els.askSubmit.textContent = label;
    }
  }

  /* -- header search ------------------------------------------------------ */

  function setActiveSearch(index) {
    state.activeSearch = index;
    state.searchOptions.forEach((option, at) => {
      option.className =
        at === index ? "wiki-search__opt wiki-search__opt--active" : "wiki-search__opt";
      option.setAttribute("aria-selected", String(at === index));
    });
    const active = state.searchOptions[index];
    if (active) {
      const id =
        typeof active.getAttribute === "function"
          ? active.getAttribute("id")
          : active.attrs.id;
      els.searchInput.setAttribute("aria-activedescendant", id);
      if (typeof active.scrollIntoView === "function") {
        active.scrollIntoView({ block: "nearest" });
      }
    } else if (typeof els.searchInput.removeAttribute === "function") {
      els.searchInput.removeAttribute("aria-activedescendant");
    }
  }

  function closeSearch() {
    els.searchResults.hidden = true;
    els.searchResults.replaceChildren();
    state.searchResults = [];
    state.searchOptions = [];
    setActiveSearch(-1);
    els.searchInput.setAttribute("aria-expanded", "false");
  }

  /** Opens the result the keyboard (or a click) chose. The server's result
   * URLs are site URLs; only one that parses back to a slug is followable. */
  function chooseResult(index) {
    const item = state.searchResults[index];
    closeSearch();
    if (!item) return;
    const where = parseBrainUrl(item.url);
    if (where && where.slug) {
      location.assign(`wiki.html?slug=${encodeURIComponent(where.slug)}`);
    }
  }

  function paintSearchMessage(message) {
    els.searchResults.replaceChildren();
    const line = document.createElement("p");
    line.className = "wiki-search__none";
    line.textContent = message;
    els.searchResults.appendChild(line);
    els.searchResults.hidden = false;
    els.searchInput.setAttribute("aria-expanded", "true");
  }

  async function runSearch(query) {
    let data;
    try {
      data = await api(searchPath(query, state.project));
    } catch (error) {
      if (error instanceof ApiError && error.status === 401) paintSearchMessage("Sign in to search.");
      else closeSearch();
      return;
    }
    if (els.searchInput.value.trim() !== query) return; // a newer keystroke won
    const results = data && Array.isArray(data.results) ? data.results : [];
    state.searchResults = results;
    state.searchOptions = [];
    els.searchResults.replaceChildren();
    if (!results.length) {
      paintSearchMessage("No results.");
      return;
    }
    const words = terms(query);
    results.forEach((item, at) => {
      const option = document.createElement("div");
      option.className = "wiki-search__opt";
      option.setAttribute("role", "option");
      option.setAttribute("id", `wiki-search-opt-${at}`);
      option.setAttribute("aria-selected", "false");
      const title = document.createElement("span");
      title.className = "wiki-search__title";
      mountNodes(firstHighlight(String((item && item.title) || ""), words), title);
      const snippet = document.createElement("span");
      snippet.className = "wiki-search__snippet";
      mountNodes(highlightTerms(String((item && item.snippet) || ""), words), snippet);
      option.append(title, snippet);
      option.addEventListener("click", () => chooseResult(at));
      els.searchResults.appendChild(option);
      state.searchOptions.push(option);
    });
    els.searchResults.hidden = false;
    els.searchInput.setAttribute("aria-expanded", "true");
    setActiveSearch(-1);
  }

  /* -- wire everything ----------------------------------------------------- */

  els.drawerToggle.addEventListener("click", () =>
    els.drawer.hidden ? openDrawer() : closeDrawer(),
  );

  if (els.drawerSignout) {
    els.drawerSignout.addEventListener("click", async () => {
      try {
        const request = signOutRequest();
        await api(request.path, { method: request.method });
      } catch {
        /* the cookie may already be gone; either way, leave for the front door */
      }
      location.assign("index.html");
    });
  }

  for (const trigger of $$("[data-new-page]")) {
    trigger.addEventListener("click", openNewPage);
  }

  if (els.searchProject) {
    els.searchProject.addEventListener("change", () => {
      state.project = validProject(els.searchProject.value) ? els.searchProject.value : "";
      // A query already typed is re-asked under the new filter at once.
      const query = els.searchInput.value.trim();
      if (query.length >= 2) runSearch(query);
    });
  }

  if (els.sessionsProject) {
    els.sessionsProject.addEventListener("change", async () => {
      state.sessionsProject = validProject(els.sessionsProject.value)
        ? els.sessionsProject.value
        : "";
      // The address names the filter, so a reload or a shared link keeps it.
      const query = state.sessionsProject
        ? `?view=sessions&project=${encodeURIComponent(state.sessionsProject)}`
        : "?view=sessions";
      history.replaceState(null, "", `wiki.html${query}`);
      state.sessionsCursor = null;
      await loadSessions();
    });
  }
  if (els.pageFactsAsOf) {
    els.pageFactsAsOf.addEventListener("change", () =>
      pageFactsAsOf(els.pageFactsAsOf.value || ""),
    );
  }
  if (els.factsAsOf) {
    els.factsAsOf.addEventListener("change", () => loadFacts(els.factsAsOf.value || ""));
  }
  if (els.factsNow) {
    els.factsNow.addEventListener("click", () => {
      if (els.factsAsOf) els.factsAsOf.value = "";
      loadFacts("");
    });
  }
  if (els.sessionsMore) {
    els.sessionsMore.addEventListener("click", () => loadSessions({ more: true }));
  }
  if (els.caProcess) els.caProcess.addEventListener("click", processNow);

  els.listFilter.addEventListener("input", () => {
    state.filter = els.listFilter.value;
    renderHome();
  });

  els.edit.addEventListener("click", openEditor);
  els.cancelEdit.addEventListener("click", leaveEditing);
  els.save.addEventListener("click", savePage);
  els.pinToggle.addEventListener("click", togglePin);

  els.editWrite.addEventListener("click", () => {
    state.previewing = false;
    paintEditMode();
  });
  els.editPreview.addEventListener("click", () => {
    state.previewing = true;
    // The preview is always the textarea's current content.
    renderPreview();
    paintEditMode();
  });

  els.npSlug.addEventListener("input", paintNpValidity);
  els.npForm.addEventListener("submit", createPage);
  els.npCancel.addEventListener("click", leaveEditing);

  els.askOpen.addEventListener("click", openAsk);
  els.askOpenEdit.addEventListener("click", openAsk);
  els.askClose.addEventListener("click", closeAsk);
  els.askSubmit.addEventListener("click", submitAsk);

  els.searchInput.addEventListener("input", () => {
    clearTimeout(searchTimer);
    const query = els.searchInput.value.trim();
    if (query.length < 2) {
      closeSearch();
      return;
    }
    searchTimer = setTimeout(() => runSearch(query), SEARCH_DEBOUNCE_MS);
  });
  els.searchInput.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      closeSearch();
      return;
    }
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      const count = state.searchOptions.length;
      if (!count) return;
      event.preventDefault();
      let next = state.activeSearch + (event.key === "ArrowDown" ? 1 : -1);
      if (next < -1) next = count - 1;
      if (next >= count) next = -1;
      setActiveSearch(next);
      return;
    }
    if (event.key === "Enter") {
      event.preventDefault();
      chooseResult(state.activeSearch >= 0 ? state.activeSearch : 0);
    }
  });
  els.searchForm.addEventListener("submit", (event) => {
    event.preventDefault();
    if (state.searchResults.length) {
      chooseResult(state.activeSearch >= 0 ? state.activeSearch : 0);
    }
  });

  // One document keydown for the two global keys: "/" focuses search (unless
  // the keystroke is aimed at a field), Escape shuts the drawer.
  document.addEventListener("keydown", (event) => {
    if (!event || typeof event.key !== "string") return;
    if (event.key === "/" && !event.metaKey && !event.ctrlKey && !event.altKey) {
      const target = event.target;
      const tag =
        target && typeof target.tagName === "string" ? target.tagName.toLowerCase() : "";
      const typing =
        tag === "input" || tag === "textarea" || tag === "select" ||
        Boolean(target && target.isContentEditable);
      if (!typing) {
        event.preventDefault();
        els.searchInput.focus();
      }
      return;
    }
    if (event.key === "Escape" && !els.drawer.hidden) closeDrawer();
  });

  // Clicks outside the search panel or the drawer close them.
  document.addEventListener("click", (event) => {
    const target = event && event.target;
    const inside = (sel) =>
      Boolean(target && typeof target.closest === "function" && target.closest(sel));
    if (!els.searchResults.hidden && !inside("#wiki-search-form")) closeSearch();
    if (!els.drawer.hidden && !inside("#wiki-drawer") && !inside("#drawer-toggle")) {
      closeDrawer();
    }
  });

  /* -- boot: session, then route ------------------------------------------ */

  state.session = await checkSession(api);
  state.signedIn = state.session.kind === "signed-in";
  paintSession();
  if (state.signedIn) await loadProjects();

  if (startSlug) await openPage(startSlug, { edit: startInEdit });
  else if (startView === "sessions" && params.get("source")) await loadTranscript(params.get("source"));
  else if (startView === "sessions") await loadSessions();
  else if (startView === "facts") await loadFacts();
  else await loadHome();
}
