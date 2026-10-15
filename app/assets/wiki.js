// The wiki page: Pages (list + pins), a page, its editor, the new-page form,
// the agent-sessions list with each session's compile status and the queue
// card, one session's transcript, the compiled `coding-agents/` folders, the
// imports, Facts, and Ask the brain — one box that searches as you type and
// asks on Enter (with its project filter) — plus the ask-about-this-page
// dialog. Which view shows is the address's (`?view=`, `?slug=`); the
// sidebar around them is `shell.js`'s (docs/ux-navigation.md).
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
import { applyHeader, checkSession } from "./session.js";
import { factsDescription, factsPath } from "./facts.js";
import { pageCrumbs, transcriptCrumbs, askHref } from "./nav.js";
import { icon } from "./shell.js";

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

/** The GitHub kinds the sources listing files under (docs/github.md). */
export const GITHUB_KINDS = Object.freeze(["github_commit", "github_issue", "github_pr"]);

/** What a GitHub kind is called in the filter and on a row. */
export const GITHUB_KIND_LABELS = Object.freeze({
  github_commit: "Commit",
  github_issue: "Issue",
  github_pr: "Pull request",
});

/** `""` (every GitHub kind) or one of `GITHUB_KINDS`. */
export function validGithubKind(value) {
  return value === "" || GITHUB_KINDS.includes(value);
}

/** One page of the GitHub listing for one kind: the project when chosen, and
 * the previous page's cursor when there is one. */
export function githubPath({ kind = "", project = "", cursor = "", limit = SESSIONS_LIMIT } = {}) {
  const params = new URLSearchParams({ kind, limit: String(limit) });
  if (validProject(project)) params.set("project", project);
  if (cursor) params.set("cursor", cursor);
  return `/v1/sources?${params.toString()}`;
}

const GITHUB_PATH_PATTERN = /^github\/([A-Za-z0-9._-]{1,100})\/([A-Za-z0-9._-]{1,100})\/(commit|issue|pr)\/([A-Za-z0-9]{1,64})$/;

/** A GitHub source's label, repository and permalink, from its listing path
 * (`github/<owner>/<repo>/commit/<sha>`, `…/issue/<n>`, `…/pr/<n>`), or null.
 * The permalink is rebuilt from the parts, so only github.com is ever linked. */
export function githubRef(path) {
  const match = GITHUB_PATH_PATTERN.exec(String(path || ""));
  if (!match) return null;
  const [, owner, repo, leaf, id] = match;
  const base = `https://github.com/${owner}/${repo}`;
  const repoName = `${owner}/${repo}`;
  if (leaf === "commit") {
    if (!/^[0-9a-f]{7,40}$/.test(id)) return null;
    return { label: id.slice(0, 7), repo: repoName, url: `${base}/commit/${id}` };
  }
  if (!/^[0-9]+$/.test(id)) return null;
  const url = leaf === "issue" ? `${base}/issues/${id}` : `${base}/pull/${id}`;
  return { label: `#${id}`, repo: repoName, url };
}

const GITHUB_URL_PATTERN = /^https:\/\/github\.com\/([A-Za-z0-9._-]{1,100}\/[A-Za-z0-9._-]{1,100})\/(commit\/[0-9a-f]{7,40}|issues\/[0-9]+|pull\/[0-9]+)(?:[#?][^\s]*)?$/;

/** The label and permalink a GitHub URL names (a citation's link), or null. */
export function githubCitation(url) {
  const match = GITHUB_URL_PATTERN.exec(String(url || ""));
  if (!match) return null;
  const [, repo, rest] = match;
  const [kind, id] = rest.split("/");
  const label = kind === "commit" ? id.slice(0, 7) : `#${id}`;
  return { repo, label, url: `https://github.com/${repo}/${rest}` };
}

/** The header search's request: `q` and `limit`, plus `project` when one is
 * chosen. With no project the URL is exactly what it always was. */
export function searchPath(query, project, limit = SEARCH_LIMIT) {
  let path = `/v1/search?q=${encodeURIComponent(query)}&limit=${limit}`;
  if (validProject(project)) path += `&project=${encodeURIComponent(project)}`;
  return path;
}

/** The agents a session can come from, as `GET /v1/sources?agent=` names
 * them (the first word of a session's title), with what a person reads. */
export const AGENT_LABELS = {
  claude_code: "Claude Code",
  codex: "Codex",
  colonizer: "Colonizer",
};

/** An agent name the sources listing accepts. */
export function validAgent(value) {
  return typeof value === "string" && /^[a-z0-9_-]{1,32}$/.test(value);
}

/** One page of the agent-sessions list: `kind=agent_log`, the project and
 * the agent when chosen, and the previous page's cursor when there is one. */
export function sessionsPath({ project = "", agent = "", cursor = "", limit = SESSIONS_LIMIT } = {}) {
  const params = new URLSearchParams({ kind: "agent_log", limit: String(limit) });
  if (validProject(project)) params.set("project", project);
  if (validAgent(agent)) params.set("agent", agent);
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

/** `1234567` as `1,234,567`. */
function grouped(n) {
  return String(Math.round(Number(n) || 0)).replace(/\B(?=(\d{3})+(?!\d))/g, ",");
}

/** How long between two timestamps, as `45s`, `3m 05s` or `1h 02m`; "" when
 * either does not read or the end comes first. */
export function formatDuration(start, end) {
  const ms = Date.parse(end) - Date.parse(start);
  if (!Number.isFinite(ms) || ms < 0) return "";
  const s = Math.floor(ms / 1000);
  const pad = (n) => String(n).padStart(2, "0");
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${pad(s % 60)}s`;
  return `${Math.floor(s / 3600)}h ${pad(Math.floor((s % 3600) / 60))}m`;
}

/** Every token a totals object counts, summed. */
function tokenSum(totals) {
  const t = totals || {};
  return (
    (Number(t.input_tokens) || 0) +
    (Number(t.output_tokens) || 0) +
    (Number(t.cache_read_tokens) || 0) +
    (Number(t.cache_creation_tokens) || 0)
  );
}

/** A record's main model: the first it names that a model wrote. */
export function mainModel(models) {
  return (Array.isArray(models) ? models : []).find((model) => model && model !== "<synthetic>") || "";
}

/** A subagent's header as text parts: name, model, duration, tokens, cost,
 * status — what is known of each. */
export function subagentHeader(sub) {
  const parts = [String((sub && sub.name) || "subagent")];
  const models = Array.isArray(sub && sub.models) ? sub.models.filter(Boolean) : [];
  if (models.length) parts.push(models.join(", "));
  const duration = sub && sub.started_at && sub.ended_at ? formatDuration(sub.started_at, sub.ended_at) : "";
  if (duration) parts.push(duration);
  const tokens = tokenSum(sub && sub.totals);
  if (tokens > 0) parts.push(`${grouped(tokens)} tokens`);
  if (sub && sub.totals && sub.totals.cost_usd != null) parts.push(`$${Number(sub.totals.cost_usd).toFixed(4)}`);
  if (sub && sub.status) parts.push(String(sub.status));
  return parts;
}

/** Where a subagent's final answer sits in its turns: the last assistant
 * turn that said something, or -1. */
export function finalAnswerIndex(turns) {
  const list = Array.isArray(turns) ? turns : [];
  for (let i = list.length - 1; i >= 0; i -= 1) {
    const turn = list[i] || {};
    if (turn.role === "assistant" && String(turn.text || "").trim()) return i;
  }
  return -1;
}

/** Every subagent a list of turns launched, depth first. */
export function subagentsOf(turns) {
  const out = [];
  for (const turn of Array.isArray(turns) ? turns : []) {
    for (const call of Array.isArray(turn && turn.tool_calls) ? turn.tool_calls : []) {
      if (call && call.subagent && typeof call.subagent === "object") {
        out.push(call.subagent);
        out.push(...subagentsOf(call.subagent.turns));
      }
    }
  }
  return out;
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

/** A subagent, as a card nested under the call that launched it: its
 * header (who, on what, how long, at what cost, how it ended), its task,
 * its final answer, and a button that draws its other turns when asked —
 * they can be hundreds. `subs` collects every card's subagent, so the page
 * can find one by the index its button carries. */
function subagentCard(sub, subs) {
  const index = subs.length;
  subs.push(sub);
  const turns = Array.isArray(sub.turns) ? sub.turns : [];
  const answerAt = finalAnswerIndex(turns);
  const steps = turns.length - (answerAt === -1 ? 0 : 1);
  const [name, ...rest] = subagentHeader(sub);
  const status = String(sub.status || "");
  const head = [tx("🤖 "), el("strong", [tx(name)])];
  const facts = status ? rest.slice(0, -1) : rest;
  if (facts.length) head.push(tx(` · ${facts.join(" · ")}`));
  if (status) {
    head.push(
      el("span", [tx(` · ${status}`)], {
        class: `transcript-sub__status${status === "failed" ? " is-bad" : ""}`,
      }),
    );
  }
  const children = [el("div", head, { class: "transcript-sub__head" })];
  if (sub.description) children.push(el("p", [tx(sub.description)], { class: "transcript-sub__desc" }));
  if (sub.truncated) {
    children.push(
      el("p", [tx(`${sub.omitted_turns || 0} of its turn(s) were left out to fit the upload size cap.`)], {
        class: "transcript-note",
      }),
    );
  }
  if (answerAt !== -1) {
    children.push(el("div", renderMarkdown(String(turns[answerAt].text)), { class: "transcript-md transcript-sub__answer" }));
  }
  if (steps > 0) {
    children.push(
      el("button", [tx(`Show ${steps} step${steps === 1 ? "" : "s"}`)], {
        type: "button",
        class: "transcript-sub__more",
        "data-sub": String(index),
        "aria-expanded": "false",
      }),
    );
    children.push(el("div", [], { class: "transcript-sub__steps", "data-sub-steps": String(index), hidden: "" }));
  }
  return el("div", children, { class: "transcript-sub" });
}

/** A subagent's turns other than its final answer, drawn the way the
 * session's are — its own subagents nested again. */
export function subagentStepNodes(sub, subs = []) {
  const turns = Array.isArray(sub && sub.turns) ? sub.turns : [];
  const answerAt = finalAnswerIndex(turns);
  return turnListNodes(
    turns.filter((_, i) => i !== answerAt),
    mainModel(sub && sub.models),
    subs,
  );
}

/** One tool call as node descriptions: name, file and outcome on a line,
 * the input fenced, the result folded away when it is long. A call that
 * launched a subagent shows its prompt folded and the subagent's card in
 * place of a result. */
function callNodes(call, subs = []) {
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
  if (call.subagent && typeof call.subagent === "object") {
    if (call.input) {
      children.push(
        el("details", [el("summary", [tx("Prompt")]), el("pre", [el("code", [tx(call.input)])], { "data-lang": "text" })], {
          class: "transcript-call__output",
        }),
      );
    }
    children.push(subagentCard(call.subagent, subs));
    return el("div", children, { class: "transcript-call" });
  }
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

/** One turn as a node description, or null when it shows nothing. `badge`
 * is the model to name on it — set where the model changed. */
function turnNodes(turn, subs = [], badge = "") {
  const role = turn.role || "assistant";
  const text = String(turn.text || "");
  const at = turn.at ? shortTime(turn.at) : "";
  if (role === "event") {
    return el("p", [tx(`— ${text}${at ? ` · ${at}` : ""} —`)], { class: "transcript-event" });
  }
  const label = { user: "User", assistant: "Assistant", reasoning: "Reasoning" }[role] || role;
  const headParts = [el("strong", [tx(label)]), tx(at ? ` · ${at}` : "")];
  if (badge) headParts.push(tx(" "), el("span", [tx(badge)], { class: "pill transcript-model" }));
  const head = el("div", headParts, {
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
    body.push(callNodes(call || {}, subs));
  }
  if (!body.length) return null;
  return el("div", [text.trim() || badge ? head : null, ...body].filter(Boolean), {
    class: `transcript-turn transcript-turn--${role}`,
  });
}

/** A list of turns, a model badge wherever a turn's model differs from the
 * one before it (starting from `model`, the list's own). */
function turnListNodes(turns, model, subs) {
  const nodes = [];
  let current = model || "";
  for (const turn of turns || []) {
    let badge = "";
    if (turn && turn.role === "assistant" && turn.model && turn.model !== current) {
      badge = String(turn.model);
      current = turn.model;
    }
    const node = turnNodes(turn || {}, subs, badge);
    if (node) nodes.push(node);
  }
  return nodes;
}

/** The whole transcript as node descriptions: the header facts, then every
 * turn in order, each subagent as a card in the call that launched it
 * (its steps drawn on request: `subs` collects the cards' subagents for
 * that). Pure, so the tests read it without a page. */
export function transcriptNodes(record, subs = []) {
  const stats = sessionStats(record);
  const totals = record.totals || {};
  const subagents = subagentsOf(record.turns);
  const kinds = new Map();
  for (const sub of subagents) kinds.set(sub.name || "subagent", (kinds.get(sub.name || "subagent") || 0) + 1);
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
    ["Model", mainModel(record.models)],
    [
      "Models",
      Array.isArray(record.models) && record.models.filter((m) => m !== "<synthetic>").length > 1
        ? record.models.filter((m) => m !== "<synthetic>").join(", ")
        : "",
    ],
    [
      "Subagents",
      subagents.length
        ? `${subagents.length} (${[...kinds].map(([name, n]) => `${name} ×${n}`).join(", ")})`
        : "",
    ],
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
    const parts = [];
    if (record.omitted_turns) parts.push(`${record.omitted_turns} turn(s)`);
    if (record.omitted_subagent_turns) parts.push(`${record.omitted_subagent_turns} subagent turn(s)`);
    nodes.push(
      el(
        "p",
        [
          tx(
            parts.length
              ? `${parts.join(" and ")} were left out to fit the upload size cap.`
              : "Part of this session was left out to fit the upload size cap.",
          ),
        ],
        { class: "transcript-note" },
      ),
    );
  }
  nodes.push(...turnListNodes(record.turns, mainModel(record.models), subs));
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

/** A project's folder name, as the compiler folds it (layout.rs
 * `segment`): lowercase ASCII letters and digits, every other run one `-`,
 * at most 28 bytes, `no-project` when nothing survives. */
export function projectSegment(project) {
  let out = "";
  let dash = false;
  for (const ch of String(project || "")) {
    const lower = ch.toLowerCase();
    if (/^[a-z0-9]$/.test(lower)) {
      if (dash && out) out += "-";
      dash = false;
      out += lower;
    } else {
      dash = true;
    }
    if (out.length >= 28) break;
  }
  out = out.replace(/^-+|-+$/g, "");
  return out || "no-project";
}

/** The collection's pages out of a page listing, as a folder tree:
 * `{ top, agents: [{ agent, pages, projects: [{ project, pages }] }], rollups }`,
 * each page `{ row, place }`. Rows outside the collection are left out.
 *
 * `sessions` is `GET /v1/coding-agents/tree`'s answer, when there is one:
 * every agent and project the asker synced sessions under becomes a folder
 * even before a page is compiled into it, and carries `info` — its record
 * agent name, sessions, compiled and queue statuses — for the folder's line
 * and its link to the Agent sessions list. */
export function codingAgentsTree(rows, sessions = null) {
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
  for (const entry of (sessions && Array.isArray(sessions.agents) ? sessions.agents : [])) {
    if (!entry || !entry.agent) continue;
    const folder = agent(String(entry.folder || entry.agent));
    folder.info = {
      agent: String(entry.agent),
      sessions: Number(entry.sessions) || 0,
      compiled: Number(entry.compiled) || 0,
    };
    for (const project of Array.isArray(entry.projects) ? entry.projects : []) {
      const name = project && project.project ? String(project.project) : "";
      const key = projectSegment(name);
      if (!folder.projects.has(key)) folder.projects.set(key, { project: key, pages: [] });
      const node = folder.projects.get(key);
      const before = node.info || { agent: String(entry.agent), project: name, sessions: 0, compiled: 0, statuses: {} };
      const statuses = { ...before.statuses };
      for (const [status, n] of Object.entries((project && project.statuses) || {})) {
        statuses[status] = (statuses[status] || 0) + (Number(n) || 0);
      }
      node.info = {
        agent: before.agent,
        project: before.project || name,
        sessions: before.sessions + (Number(project.sessions) || 0),
        compiled: before.compiled + (Number(project.compiled) || 0),
        statuses,
      };
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
        info: folder.info || null,
        pages: folder.pages.sort(byPage),
        projects: [...folder.projects.values()]
          .sort((a, b) => a.project.localeCompare(b.project))
          .map((project) => ({
            project: project.project,
            info: project.info || null,
            pages: project.pages.sort(byPage),
          })),
      })),
  };
}

/** A synced folder's line: `12 sessions · 3 compiled · 9 queued`. */
export function folderSummary(info) {
  if (!info) return "";
  const parts = [`${info.sessions} session${info.sessions === 1 ? "" : "s"}`, `${info.compiled} compiled`];
  for (const status of Object.keys(JOB_LABELS)) {
    if (status === "done" || status === "superseded") continue;
    const n = Number((info.statuses && info.statuses[status]) || 0);
    if (n > 0) parts.push(`${n} ${jobLabel(status).toLowerCase()}`);
  }
  return parts.join(" · ");
}

/** The Agent sessions list, filtered to one agent and (optionally) one
 * project: where a synced folder links. */
export function sessionsHref(agent = "", project = "") {
  const params = new URLSearchParams({ view: "sessions" });
  if (validAgent(agent)) params.set("agent", agent);
  if (validProject(project)) params.set("project", project);
  return `wiki.html?${params.toString()}`;
}

/** The compile queue's tree route: synced sessions by agent and project. */
export const TREE_PATH = "/v1/coding-agents/tree";

/** The compile queue's status route, optionally for one source. */
export function queuePath(source = "") {
  return source
    ? `/v1/coding-agents/queue?source=${encodeURIComponent(source)}`
    : "/v1/coding-agents/queue?limit=100";
}

/**
 * `POST /v1/ask`, waiting for the asker's own runner when they run their jobs
 * on their device: the server answers `202 {status: "pending", poll,
 * retry_after}`, the job is polled until it ends, and the same ask is sent
 * again, which reads the runner's answer. `onWait` runs once, when the wait
 * starts; after `maxWaitMs` the wait gives up with an error that says what it
 * was waiting for. `apiFn` and `sleep` are parameters so a test can drive it.
 */
export async function askWithRunner(apiFn, body, options = {}) {
  const {
    onWait = () => {},
    sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms)),
    maxWaitMs = 300_000,
  } = options;
  const ask = () => apiFn("/v1/ask", { method: "POST", body });
  const first = await ask();
  if (!first || first.status !== "pending") return first;
  const poll = typeof first.poll === "string" ? first.poll : "";
  if (!poll.startsWith("/v1/model-jobs/")) {
    throw new Error("the server named no job to wait for");
  }
  onWait();
  const pause = Math.min(Math.max(Number(first.retry_after) || 2, 1), 30) * 1000;
  let waited = 0;
  for (;;) {
    if (waited >= maxWaitMs) {
      throw new Error(
        "your device did not answer in time — is “Run brain jobs with my subscription” on, or `livingbrain jobs run` running?",
      );
    }
    await sleep(pause);
    waited += pause;
    let job = null;
    try {
      job = await apiFn(poll);
    } catch {
      job = null;
    }
    const status = job && job.status;
    if (status !== "queued" && status !== "claimed") return ask();
  }
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
  "skipped-trivial": "Skipped (trivial)",
  "summary-only": "Summary only",
};

/** A token count a person reads at a glance: 900, 12k, 1.4M. */
function shortCount(value) {
  const n = Number(value) || 0;
  const trim = (x) => String(Number(x.toFixed(1)));
  if (n >= 1_000_000) return `${trim(n / 1_000_000)}M`;
  if (n >= 1_000) return `${trim(n / 1_000)}k`;
  return String(Math.round(n));
}

/**
 * The queue's triage block as one line: how many sessions triage skipped,
 * summarised and compiled in full, and the tokens it and compaction saved
 * (an estimate of input tokens). Empty when triage has decided nothing yet.
 */
export function triageSummary(triage) {
  if (!triage || typeof triage !== "object") return "";
  const skipped = Number(triage.skipped) || 0;
  const summarised = Number(triage.summarised) || 0;
  const full = Number(triage.full) || 0;
  const off = triage.enabled === false ? " Triage is off." : "";
  if (skipped + summarised + full === 0) return off.trim();
  const parts = [];
  if (skipped) parts.push(`${skipped} skipped`);
  if (summarised) parts.push(`${summarised} summary only`);
  if (full) parts.push(`${full} compiled in full`);
  const saved = Number(triage.tokens_saved) || 0;
  const raw = Number(triage.tokens_raw) || 0;
  const share = raw > 0 ? ` (${Math.round((saved / raw) * 100)}%)` : "";
  const by = triage.classifier ? " Triage by Jev." : "";
  return `Triage: ${parts.join(" · ")} · ~${shortCount(saved)} tokens saved${share}.${by}${off}`;
}

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

/**
 * Replaces every `[[slug]]` / `[[slug|Label]]` in the article's text with a
 * real anchor: a `TreeWalker` over the text nodes, each swapped in place
 * for text nodes and anchors built with `createElement` + `textContent`,
 * like everything else that reaches the document. A body that never yields
 * a valid slug — `[[orbit window]]` bare, say — keeps the literal brackets
 * it was written with, and `pre`/`code` are never touched: there `[[…]]` is
 * quoted text, not a link.
 */
export function linkWikilinks(container) {
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
  const startAgent = validAgent(params.get("agent")) ? params.get("agent") : "";

  const els = {
    views: {
      home: $("#home-view"),
      page: $("#page-view"),
      editor: $("#editor-view"),
      new: $("#new-page-view"),
      sessions: $("#sessions-view"),
      transcript: $("#transcript-view"),
      facts: $("#facts-view"),
      ask: $("#ask-view"),
      "coding-agents": $("#coding-agents-view"),
      imports: $("#imports-view"),
      github: $("#github-view"),
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
    homeCaHint: $("#home-ca-hint"),
    pageCrumbs: $("#page-crumbs"),
    transcriptCrumbs: $("#transcript-crumbs"),
    askGo: $("#ask-go"),
    askViewStatus: $("#ask-view-status"),
    askViewResult: $("#ask-view-result"),
    askViewAnswer: $("#ask-view-answer"),
    askViewCitations: $("#ask-view-citations"),
    caStatus: $("#ca-status"),
    caEmpty: $("#ca-empty"),
    importsStatus: $("#imports-status"),
    importsList: $("#imports-list"),
    importsMore: $("#imports-more"),
    importsEmpty: $("#imports-empty"),
    githubKind: $("#github-kind"),
    githubProject: $("#github-project"),
    githubStatus: $("#github-status"),
    githubList: $("#github-list"),
    githubMore: $("#github-more"),
    githubEmpty: $("#github-empty"),
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
    sessionsAgent: $("#sessions-agent"),
    sessionsStatus: $("#sessions-status"),
    sessionsList: $("#sessions-list"),
    sessionsEmpty: $("#sessions-empty"),
    sessionsMore: $("#sessions-more"),
    caCard: $("#ca-card"),
    caSummary: $("#ca-summary"),
    caModel: $("#ca-model"),
    caProcess: $("#ca-process"),
    caRetry: $("#ca-retry"),
    caTriage: $("#ca-triage"),
    caResult: $("#ca-result"),
    caSection: $("#coding-agents-section"),
    caTree: $("#coding-agents-tree"),
    transcriptCompile: $("#transcript-compile"),
    transcriptTitle: $("#transcript-title"),
    transcriptStatus: $("#transcript-status"),
    transcriptBody: $("#transcript-body"),
    drawerToggle: $("#drawer-toggle"),
    drawer: $("#wiki-drawer"),
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
    sessionsAgent: startAgent,
    sessionsCursor: null,
    // The synced sessions by agent and project (`/v1/coding-agents/tree`).
    caSessions: null,
    // The compile queue, keyed by source id (`/v1/coding-agents/queue`).
    jobs: new Map(),
    // The view on screen, as `showView` last set it.
    view: "",
    // The imports listing (`/v1/sources?kind=import`) and its cursor.
    imports: [],
    importsCursor: null,
    // The GitHub listing (`/v1/sources?kind=github_*`): kind, project, cursor.
    github: [],
    githubCursor: null,
    githubKind: validGithubKind(params.get("kind")) ? params.get("kind") : "",
    githubProject: startProject,
  };
  let searchTimer = null;

  /* -- small shared pieces ------------------------------------------ */

  const VIEWS = [
    "home", "page", "editor", "new", "sessions", "transcript", "facts",
    "ask", "coding-agents", "imports", "github",
  ];

  /** The tab title per view; a page's own title replaces it when it loads. */
  const VIEW_TITLES = {
    home: "Pages",
    new: "New page",
    sessions: "Agent sessions",
    transcript: "Agent session",
    facts: "Facts",
    ask: "Ask the brain",
    "coding-agents": "Coding agents",
    imports: "Imports",
  };

  /** Shows one view, and tells the shell (the sidebar's current item) and
   * the sheet (`body[data-view]`) which it is. */
  function showView(name) {
    state.view = name;
    for (const view of VIEWS) {
      if (els.views[view]) els.views[view].hidden = view !== name;
    }
    if (document.body && document.body.dataset) document.body.dataset.view = name;
    if (VIEW_TITLES[name]) document.title = `${VIEW_TITLES[name]} — Living Brain`;
    if (typeof CustomEvent === "function" && typeof document.dispatchEvent === "function") {
      document.dispatchEvent(new CustomEvent("lb-view", { detail: { view: name } }));
    }
  }

  /** A breadcrumb trail (`nav.js`'s `[{label, href?}]`) into its `<nav>`:
   * an ordered list, links for the ancestors, the current page last. */
  function paintCrumbs(container, crumbs) {
    if (!container) return;
    const items = crumbs.map((crumb, at) => {
      const last = at === crumbs.length - 1;
      const inner = crumb.href && !last
        ? el("a", [tx(crumb.label)], { href: crumb.href })
        : el("span", [tx(crumb.label)], last ? { "aria-current": "page" } : null);
      return el("li", [inner]);
    });
    renderInto(container, [el("ol", items, { class: "crumbs__list" })]);
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
    // The compiled coding-agents pages have their own place (Sources →
    // Coding agents): unfiltered they would crowd everything else off the
    // list, so they show here only when a filter asks for them.
    const recent = matches.filter(
      (row) => !pins.includes(pinId(row)) && (query !== "" || !codingAgentsPlace(row.slug)),
    );
    fillList(els.pinnedList, pinned, true);
    fillList(els.recentList, recent, false);
    els.pinnedSection.hidden = pinned.length === 0;
    els.recentSection.hidden = recent.length === 0;
    paintCodingAgentsHint(query === "" ? state.rows.filter((row) => codingAgentsPlace(row.slug)).length : 0);
    els.listFilter.hidden = state.rows.length === 0;
    els.homeEmpty.hidden = state.rows.length !== 0;
    els.homeNomatch.hidden = !(state.rows.length > 0 && matches.length === 0);
  }

  /** A synced folder's line: its numbers, linking to its sessions. */
  function treeInfo(info, project) {
    const li = document.createElement("li");
    li.className = "ca-folder__info";
    const link = document.createElement("a");
    link.href = sessionsHref(info.agent, project ? info.project : "");
    link.textContent = info.sessions === 1 ? "Open the session" : `Open all ${info.sessions} sessions`;
    li.appendChild(link);
    // What the queue says beyond the counts the folder's name already shows.
    const rest = folderSummary(info).split(" · ").slice(2).join(" · ");
    if (rest) li.appendChild(document.createTextNode(` · ${rest}`));
    return li;
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

  /** A folder: `<details>` with its name and its pages and subfolders —
   * and, for one synced sessions are filed in, a line with their numbers
   * linking to them (`info`; `project` says it is a project's). */
  function treeFolder(name, entries, children, open, info = null, project = false) {
    const details = document.createElement("details");
    details.className = "ca-folder";
    if (open) details.setAttribute("open", "");
    const summary = document.createElement("summary");
    summary.textContent = `${name}/`;
    if (info) {
      const meta = document.createElement("span");
      meta.className = "ca-folder__meta";
      meta.textContent = ` ${info.sessions} session${info.sessions === 1 ? "" : "s"} · ${info.compiled} compiled`;
      summary.appendChild(meta);
    }
    details.appendChild(summary);
    const list = document.createElement("ul");
    list.className = "ca-folder__list";
    if (info) list.appendChild(treeInfo(info, project));
    for (const entry of entries) list.appendChild(treeLink(entry));
    for (const child of children) {
      const li = document.createElement("li");
      li.appendChild(child);
      list.appendChild(li);
    }
    details.appendChild(list);
    return details;
  }

  /** The line under the Pages toolbar that says where the compiled pages
   * went: "12 compiled coding-agent pages are under Coding agents". */
  function paintCodingAgentsHint(count) {
    if (!els.homeCaHint) return;
    els.homeCaHint.replaceChildren();
    els.homeCaHint.hidden = count === 0;
    if (!count) return;
    els.homeCaHint.appendChild(
      document.createTextNode(
        `${count} compiled coding-agent ${count === 1 ? "page is" : "pages are"} filed under `,
      ),
    );
    const link = document.createElement("a");
    link.href = "wiki.html?view=coding-agents";
    link.textContent = "Coding agents";
    els.homeCaHint.appendChild(link);
    els.homeCaHint.appendChild(document.createTextNode("."));
  }

  /** The `coding-agents/` folders: agent, then project. */
  function renderCodingAgents(rows, filtering) {
    if (!els.caSection || !els.caTree) return;
    // Filtering narrows to pages; the synced folders are the unfiltered view.
    const tree = codingAgentsTree(rows, filtering ? null : state.caSessions);
    const empty = !tree.top.length && !tree.agents.length && !tree.rollups.length;
    els.caSection.hidden = empty;
    els.caTree.replaceChildren();
    if (empty) return;
    const agents = tree.agents.map((agent) =>
      treeFolder(
        agent.agent,
        agent.pages,
        agent.projects.map((project) =>
          treeFolder(project.project, project.pages, [], filtering, project.info, true),
        ),
        filtering || Boolean(agent.info),
        agent.info,
      ),
    );
    const folders = [...agents];
    if (tree.rollups.length) folders.push(treeFolder(ROLLUP, tree.rollups, [], filtering));
    els.caTree.appendChild(treeFolder(CODING_AGENTS, tree.top, folders, true));
  }

  function renderHomeSignedOut() {
    if (els.caSection) els.caSection.hidden = true;
    if (els.homeCaHint) els.homeCaHint.hidden = true;
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

  /* -- coding agents: the compiled folders (Sources → Coding agents) --- */

  /** The coding-agents view: the compiled pages from the page listing and
   * the synced sessions' folders from the tree route, as one folder tree
   * (#187). The tree route is a nicety: without it the folders are the
   * compiled pages alone. */
  async function loadCodingAgents() {
    showView("coding-agents");
    if (els.caEmpty) els.caEmpty.hidden = true;
    if (!state.signedIn) {
      if (els.caSection) els.caSection.hidden = true;
      if (els.caStatus) els.caStatus.textContent = "";
      signInNotice("Sign in to see your coding agents.");
      return;
    }
    if (els.caStatus) els.caStatus.textContent = "Loading…";
    try {
      const [data, sessions] = await Promise.all([
        api("/v1/pages?limit=200"),
        api(TREE_PATH).catch(() => null),
      ]);
      state.caSessions = sessions && Array.isArray(sessions.agents) ? sessions : null;
      state.rows = data && Array.isArray(data.pages) ? data.pages : [];
      renderCodingAgents(state.rows, false);
      const empty = !els.caSection || els.caSection.hidden;
      if (els.caEmpty) els.caEmpty.hidden = !empty;
      if (els.caStatus) els.caStatus.textContent = "";
      clearNotice();
    } catch (error) {
      if (els.caStatus) els.caStatus.textContent = "";
      if (error instanceof ApiError && error.status === 401) {
        state.signedIn = false;
        paintSession();
        if (els.caSection) els.caSection.hidden = true;
        signInNotice("Sign in to see your coding agents.");
        return;
      }
      notice(els.notice, `Could not load the coding agents: ${error.message}`, "bad");
      els.notice.appendChild(mkButton("Retry", loadCodingAgents));
    }
  }

  /* -- imports (Sources → Imports) ------------------------------------ */

  /** One imported source: its path, project and date; opening it reads
   * the body once (`GET /v1/sources/{id}`) and renders it as Markdown. */
  function importElement(row) {
    const li = document.createElement("li");
    li.className = "wiki-row import-row";
    const details = document.createElement("details");
    details.className = "import";
    const summary = document.createElement("summary");
    summary.className = "wiki-row__link";
    const name = document.createElement("span");
    name.className = "wiki-row__slug";
    name.textContent = row.path || row.id || "(unnamed import)";
    summary.appendChild(name);
    if (row.project) {
      const pill = document.createElement("span");
      pill.className = "pill";
      pill.textContent = row.project;
      summary.appendChild(pill);
    }
    const meta = document.createElement("span");
    meta.className = "wiki-row__meta";
    meta.textContent = [formatDate(row.created_at), row.scope === "shared" ? "shared" : ""]
      .filter(Boolean)
      .join(" · ");
    summary.appendChild(meta);
    details.appendChild(summary);
    const body = document.createElement("div");
    body.className = "prose import__body";
    details.appendChild(body);
    let loaded = false;
    details.addEventListener("toggle", async () => {
      if (!details.open || loaded) return;
      loaded = true;
      body.textContent = "Loading…";
      try {
        const source = await api(sourcePath(row.id || ""));
        renderInto(body, renderMarkdown(stripFrontmatter((source && source.body) || "")));
      } catch (error) {
        loaded = false;
        body.textContent = `Could not open this import: ${error.message}`;
      }
    });
    li.appendChild(details);
    return li;
  }

  async function loadImports({ more = false } = {}) {
    showView("imports");
    if (!state.signedIn) {
      els.importsList.hidden = true;
      els.importsEmpty.hidden = true;
      els.importsMore.hidden = true;
      els.importsStatus.textContent = "";
      signInNotice("Sign in to see your imports.");
      return;
    }
    const cursor = more ? state.importsCursor : "";
    els.importsStatus.textContent = "Loading…";
    try {
      const params = new URLSearchParams({ kind: "import", limit: String(SESSIONS_LIMIT) });
      if (cursor) params.set("cursor", cursor);
      const data = await api(`/v1/sources?${params.toString()}`);
      const rows = data && Array.isArray(data.sources) ? data.sources : [];
      state.imports = more ? state.imports.concat(rows) : rows;
      state.importsCursor = (data && data.next_cursor) || null;
      if (!more) els.importsList.replaceChildren();
      for (const row of rows) els.importsList.appendChild(importElement(row));
      els.importsList.hidden = state.imports.length === 0;
      els.importsEmpty.hidden = state.imports.length !== 0;
      els.importsMore.hidden = !state.importsCursor;
      const count = state.imports.length;
      els.importsStatus.textContent = count
        ? `${count}${state.importsCursor ? "+" : ""} ${count === 1 ? "import" : "imports"}`
        : "";
      clearNotice();
    } catch (error) {
      els.importsStatus.textContent = "";
      if (error instanceof ApiError && error.status === 401) {
        state.signedIn = false;
        paintSession();
        await loadImports();
        return;
      }
      notice(els.notice, `Could not load the imports: ${error.message}`, "bad");
      els.notice.appendChild(mkButton("Retry", () => loadImports({ more })));
    }
  }

  /* -- GitHub (Sources → GitHub) -------------------------------------- */

  /** One GitHub item: its icon, the repository and `#123` or the short sha,
   * a link to its permalink on github.com, the project and the date. */
  function githubElement(row) {
    const ref = githubRef(row.path);
    const li = document.createElement("li");
    li.className = "wiki-row github-row";
    const line = document.createElement(ref ? "a" : "span");
    line.className = "wiki-row__link";
    if (ref) {
      line.href = ref.url;
      line.setAttribute("rel", "noopener noreferrer");
    }
    const mark = icon(document, "github");
    if (mark) line.appendChild(mark);
    const name = document.createElement("span");
    name.className = "wiki-row__slug";
    name.textContent = ref ? `${ref.repo} ${ref.label}` : row.path || row.id || "(unnamed GitHub item)";
    line.appendChild(name);
    if (row.project) {
      const pill = document.createElement("span");
      pill.className = "pill";
      pill.textContent = row.project;
      line.appendChild(pill);
    }
    const meta = document.createElement("span");
    meta.className = "wiki-row__meta";
    meta.textContent = [GITHUB_KIND_LABELS[row.kind] || "GitHub item", formatDate(row.created_at)]
      .filter(Boolean)
      .join(" · ");
    line.appendChild(meta);
    li.appendChild(line);
    return li;
  }

  /** Lists the chosen GitHub kind (or each kind, newest first, when none is
   * chosen: one page of each). Only one kind pages with "Load more". */
  async function loadGithub({ more = false } = {}) {
    showView("github");
    if (!state.signedIn) {
      els.githubList.hidden = true;
      els.githubEmpty.hidden = true;
      els.githubMore.hidden = true;
      els.githubStatus.textContent = "";
      signInNotice("Sign in to see your GitHub items.");
      return;
    }
    const kind = state.githubKind;
    const project = state.githubProject;
    const cursor = more && kind ? state.githubCursor : "";
    els.githubStatus.textContent = "Loading…";
    try {
      const kinds = kind ? [kind] : GITHUB_KINDS;
      const answers = await Promise.all(
        kinds.map((each) => api(githubPath({ kind: each, project, cursor }))),
      );
      const rows = answers.flatMap((data) => (data && Array.isArray(data.sources) ? data.sources : []));
      rows.sort((a, b) => (Date.parse(b.created_at) || 0) - (Date.parse(a.created_at) || 0));
      state.github = more ? state.github.concat(rows) : rows;
      state.githubCursor = kind ? (answers[0] && answers[0].next_cursor) || null : null;
      if (!more) els.githubList.replaceChildren();
      for (const row of rows) els.githubList.appendChild(githubElement(row));
      els.githubList.hidden = state.github.length === 0;
      els.githubEmpty.hidden = state.github.length !== 0;
      els.githubMore.hidden = !state.githubCursor;
      const count = state.github.length;
      els.githubStatus.textContent = count
        ? `${count}${state.githubCursor ? "+" : ""} ${count === 1 ? "item" : "items"}${kind ? "" : " · newest first, one page of each kind"}`
        : "";
      clearNotice();
    } catch (error) {
      els.githubStatus.textContent = "";
      if (error instanceof ApiError && error.status === 401) {
        state.signedIn = false;
        paintSession();
        await loadGithub();
        return;
      }
      notice(els.notice, `Could not load the GitHub items: ${error.message}`, "bad");
      els.notice.appendChild(mkButton("Retry", () => loadGithub({ more })));
    }
  }

  /* -- ask the brain (the one box) ------------------------------------ */

  /** The Ask view; `question` pre-fills the box and asks at once. */
  async function openAskView(question = "") {
    showView("ask");
    els.askViewResult.hidden = true;
    els.askViewStatus.textContent = "";
    if (!state.signedIn) {
      signInNotice("Sign in to search and ask the brain.");
      return;
    }
    clearNotice();
    if (question) {
      els.searchInput.value = question;
      await askInView(question);
    }
    if (typeof els.searchInput.focus === "function") els.searchInput.focus();
  }

  /** Asks the brain from the Ask view and draws the answer with its
   * citations under the box. The address keeps the question (`&q=`), so a
   * reload or a shared link asks it again. */
  async function askInView(question) {
    const q = String(question || "").trim();
    if (!q) {
      els.askViewStatus.textContent = "Type a question first.";
      return;
    }
    closeSearch();
    history.replaceState(null, "", askHref(q));
    els.askGo.disabled = true;
    els.askViewStatus.textContent = "Asking…";
    try {
      const answer = await askWithRunner(api, { question: q }, {
        onWait: () => {
          els.askViewStatus.textContent =
            "Your own Claude Code or Codex is answering this on your device…";
        },
      });
      els.askViewStatus.textContent = "";
      renderInto(els.askViewAnswer, renderMarkdown((answer && answer.answer) || ""));
      linkWikilinks(els.askViewAnswer);
      renderInto(els.askViewCitations, askCitations(answer && answer.citations));
      els.askViewResult.hidden = false;
    } catch (error) {
      if (error instanceof ApiError && error.status === 401) {
        state.signedIn = false;
        paintSession();
        els.askViewStatus.textContent = "";
        signInNotice("Sign in to search and ask the brain.");
        return;
      }
      els.askViewStatus.textContent = `Could not ask: ${error.message}`;
    } finally {
      els.askGo.disabled = false;
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

  /** Fills the agent filter: "All agents", the agents this build knows,
   * and any other the tree names; keeps `selected` when it is one. */
  function fillAgentSelect(select, selected) {
    if (!select) return;
    select.replaceChildren();
    const names = new Map(Object.entries(AGENT_LABELS));
    for (const entry of (state.caSessions && state.caSessions.agents) || []) {
      if (entry && validAgent(entry.agent) && !names.has(entry.agent)) names.set(entry.agent, entry.agent);
    }
    if (validAgent(selected) && !names.has(selected)) names.set(selected, selected);
    const all = document.createElement("option");
    all.value = "";
    all.textContent = "All agents";
    select.appendChild(all);
    for (const [value, label] of names) {
      const option = document.createElement("option");
      option.value = value;
      option.textContent = label;
      select.appendChild(option);
    }
    select.value = validAgent(selected) ? selected : "";
    select.hidden = false;
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
    fillProjectSelect(els.githubProject, state.githubProject);
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
    if (job && job.can_compile_fully) {
      // Triage held this one back; its source is whole, so it can still be
      // compiled — in full, with no compaction.
      const full = document.createElement("button");
      full.type = "button";
      full.className = "ca-compile-fully";
      full.textContent = "Compile fully";
      full.setAttribute("data-compile-fully", row.id || "");
      li.appendChild(full);
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
    if (els.caTriage) {
      const line = triageSummary(data.triage);
      els.caTriage.textContent = line;
      els.caTriage.hidden = line === "";
    }
    if (els.caRetry) els.caRetry.hidden = !((data.counts && data.counts.failed) > 0);
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
    if (budget.paused) {
      els.caModel.appendChild(
        document.createTextNode(" Compiling is paused for this workspace — see "),
      );
      const limits = document.createElement("a");
      limits.href = "settings.html#limits";
      limits.textContent = "Settings → Usage & limits";
      els.caModel.appendChild(limits);
      els.caModel.appendChild(document.createTextNode("."));
    } else if (budget.daily_tokens != null) {
      // The workspace's effective budget: its own setting, else the default.
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
      // A failure names its reason — what the provider said, e.g.
      // `model-http 429: {"error":…}` — so the cause is on screen.
      els.caResult.textContent = done.length
        ? done
            .map((job) => `${jobLabel(job.status)}${job.error ? ` (${job.error})` : ""}`)
            .join(", ")
        : "Nothing waiting.";
    } catch (error) {
      els.caResult.textContent = `Could not compile: ${error.message}`;
    } finally {
      els.caProcess.disabled = false;
    }
    await loadQueue();
    renderSessions();
  }

  /** `Compile fully`: one triaged session, compiled in full in the request. */
  async function compileFully(button) {
    const source = button.getAttribute("data-compile-fully") || "";
    if (!source) return;
    button.disabled = true;
    els.caResult.textContent = "Compiling in full…";
    try {
      const data = await api("/v1/coding-agents/compile-fully", {
        method: "POST",
        body: { source_id: source },
      });
      const done = data && Array.isArray(data.processed) ? data.processed : [];
      els.caResult.textContent = done.length
        ? done
            .map((job) => `${jobLabel(job.status)}${job.error ? ` (${job.error})` : ""}`)
            .join(", ")
        : "Requeued.";
    } catch (error) {
      els.caResult.textContent = `Could not compile: ${error.message}`;
      button.disabled = false;
    }
    await loadQueue();
    renderSessions();
  }

  /** `Retry failed`: put the asker's failed jobs back in the queue with
   * their attempts reset, then refresh the card and the list. */
  async function retryFailed() {
    if (!els.caRetry) return;
    els.caRetry.disabled = true;
    try {
      const data = await api("/v1/coding-agents/retry", { method: "POST", body: {} });
      const n = (data && data.requeued) || 0;
      els.caResult.textContent = n === 1 ? "1 session requeued." : `${n} sessions requeued.`;
    } catch (error) {
      els.caResult.textContent = `Could not requeue: ${error.message}`;
    } finally {
      els.caRetry.disabled = false;
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
      if (els.sessionsAgent) els.sessionsAgent.hidden = true;
      els.sessionsStatus.textContent = "";
      signInNotice("Sign in to see your agent sessions.");
      return;
    }
    const cursor = more ? state.sessionsCursor : "";
    fillAgentSelect(els.sessionsAgent, state.sessionsAgent);
    try {
      const data = await api(
        sessionsPath({ project: state.sessionsProject, agent: state.sessionsAgent, cursor }),
      );
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

  /** Each subagent card's "Show steps" button: draws that subagent's turns
   * the first time it is pressed (they can be hundreds, so never before),
   * then shows and hides them. Nested cards drawn that way are wired too. */
  function wireSubagents(container, subs) {
    for (const button of container.querySelectorAll("[data-sub]")) {
      if (button.dataset.wired) continue;
      button.dataset.wired = "1";
      const label = button.textContent;
      button.addEventListener("click", () => {
        const index = button.getAttribute("data-sub");
        const sub = subs[Number(index)];
        const steps = Array.from(button.parentNode.children).find(
          (child) => child.getAttribute && child.getAttribute("data-sub-steps") === index,
        );
        if (!sub || !steps) return;
        const open = button.getAttribute("aria-expanded") === "true";
        if (!open && !steps.dataset.filled) {
          renderInto(steps, subagentStepNodes(sub, subs));
          steps.dataset.filled = "1";
          wireSubagents(steps, subs);
        }
        steps.hidden = open;
        button.setAttribute("aria-expanded", String(!open));
        button.textContent = open ? label : "Hide steps";
      });
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
    paintCrumbs(els.transcriptCrumbs, transcriptCrumbs("Agent session"));
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
        const subs = [];
        renderInto(els.transcriptBody, transcriptNodes(record, subs));
        wireSubagents(els.transcriptBody, subs);
        await paintTranscriptCompile(id);
      } else {
        renderInto(els.transcriptBody, [
          el("pre", [el("code", [tx((source && source.body) || "")])]),
        ]);
      }
      els.transcriptStatus.textContent = "";
      paintCrumbs(els.transcriptCrumbs, transcriptCrumbs(els.transcriptTitle.textContent));
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

  function renderPage() {
    const page = state.page || {};
    els.pageBody.hidden = false;
    els.pageMeta.hidden = false;
    els.pageTitle.textContent = page.title || page.slug || "Untitled";
    document.title = `${page.title || page.slug || "Untitled"} — Living Brain`;
    paintCrumbs(els.pageCrumbs, pageCrumbs(page.slug || state.slug, page.title));
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
    paintCrumbs(els.pageCrumbs, pageCrumbs(targetSlug, targetSlug));
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
      const github = githubCitation(url);
      // A GitHub item names itself by repository and number (or short sha),
      // and links to its permalink on github.com.
      const children = [
        github
          ? el("a", [tx(`GitHub ${github.repo} ${github.label}`)], {
              href: github.url,
              rel: "noopener noreferrer",
              class: "gh-link",
            })
          : isSafeHref(url)
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
      const answer = await askWithRunner(
        api,
        { question },
        {
          onWait: () => {
            els.askSubmit.textContent = "Waiting for your device…";
            els.askStatus.textContent =
              "Your own Claude Code or Codex is answering this on your device.";
          },
        },
      );
      els.askStatus.textContent = "";
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

  /** The sessions list again under the filters now chosen; the address
   * names them, so a reload or a shared link keeps them. */
  async function refilterSessions() {
    history.replaceState(null, "", sessionsHref(state.sessionsAgent, state.sessionsProject));
    state.sessionsCursor = null;
    await loadSessions();
  }
  if (els.sessionsProject) {
    els.sessionsProject.addEventListener("change", async () => {
      state.sessionsProject = validProject(els.sessionsProject.value)
        ? els.sessionsProject.value
        : "";
      await refilterSessions();
    });
  }
  if (els.sessionsAgent) {
    els.sessionsAgent.addEventListener("change", async () => {
      state.sessionsAgent = validAgent(els.sessionsAgent.value) ? els.sessionsAgent.value : "";
      await refilterSessions();
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
  if (els.importsMore) {
    els.importsMore.addEventListener("click", () => loadImports({ more: true }));
  }
  if (els.githubKind) {
    els.githubKind.addEventListener("change", () => {
      const value = els.githubKind.value;
      state.githubKind = validGithubKind(value) ? value : "";
      loadGithub();
    });
  }
  if (els.githubProject) {
    els.githubProject.addEventListener("change", () => {
      const value = els.githubProject.value;
      state.githubProject = validProject(value) ? value : "";
      loadGithub();
    });
  }
  if (els.githubMore) {
    els.githubMore.addEventListener("click", () => loadGithub({ more: true }));
  }
  if (els.caProcess) els.caProcess.addEventListener("click", processNow);
  if (els.caRetry) els.caRetry.addEventListener("click", retryFailed);
  if (els.sessionsList) {
    els.sessionsList.addEventListener("click", (event) => {
      const button = event.target.closest && event.target.closest("[data-compile-fully]");
      if (button) compileFully(button);
    });
  }

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
      submitBox();
    }
  });
  /** The one box's Enter (and its Ask button): a page picked with the
   * arrows opens; otherwise the text is a question for the brain. */
  function submitBox() {
    if (state.activeSearch >= 0 && state.searchResults[state.activeSearch]) {
      chooseResult(state.activeSearch);
      return;
    }
    clearTimeout(searchTimer);
    askInView(els.searchInput.value);
  }
  els.searchForm.addEventListener("submit", (event) => {
    event.preventDefault();
    submitBox();
  });

  // One document keydown for the two page keys: on the Ask view "/" focuses
  // its box (unless the keystroke is aimed at a field; everywhere else the
  // shell's palette takes "/"), and Escape shuts the drawer.
  document.addEventListener("keydown", (event) => {
    if (!event || typeof event.key !== "string") return;
    if (event.key === "/" && !event.metaKey && !event.ctrlKey && !event.altKey) {
      if (state.view !== "ask") return;
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

  if (els.githubKind) els.githubKind.value = state.githubKind;
  state.session = await checkSession(api);
  state.signedIn = state.session.kind === "signed-in";
  paintSession();
  if (state.signedIn) await loadProjects();

  if (startSlug) await openPage(startSlug, { edit: startInEdit });
  else if (startView === "sessions" && params.get("source")) await loadTranscript(params.get("source"));
  else if (startView === "sessions") await loadSessions();
  else if (startView === "facts") await loadFacts();
  else if (startView === "ask") await openAskView(String(params.get("q") || "").trim());
  else if (startView === "coding-agents") await loadCodingAgents();
  else if (startView === "imports") await loadImports();
  else if (startView === "github") await loadGithub();
  else if (startView === "new" && state.signedIn) {
    await loadHome();
    openNewPage();
  } else await loadHome();
}
