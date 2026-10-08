// The settings request builder — the one place a settings change is described.
//
// `settingRequest(section, key, value)` is pure: it turns a section, a key and
// a value into the exact request that would go to the API, or into a marked
// "not wired yet" descriptor. `app.js` calls it and sends the result, and
// nothing else in the app is allowed to mutate a setting: the pages have no
// other path to the API for settings.
//
// The body always carries `section`, `key` and `value` next to the change
// itself, so a server (or a proxy in front of it) can attribute every write to
// a named setting without parsing the URL — that is what makes the settings
// auditable, and it is why the shape lives here, in a pure function, where the
// tests can assert it.

import stack from "./stack.json" with { type: "json" };
import { isSafeHref } from "./markdown.js";

/** The sections the settings page renders, in the order the page shows them. */
export const SECTIONS = [
  "models",
  "proactivity",
  "tools",
  "automations",
  "skills",
  "colonizer",
  "stack",
];

/**
 * Which sections have a server route today.
 *
 * `models` is the only one: `PUT /v1/models/{role}` connects or replaces a
 * model connection for a role (`triage`, `main`, `research`), admin or owner
 * only. The other six are on the page because the issue asks for them and
 * because a section with no route must look *unavailable*, not broken — so
 * they render as disabled controls rather than as buttons that 404. `stack` is
 * read-only: it is vendored data shown as it is, and nothing is ever sent.
 */
const ENDPOINTS = {
  models: (key) => ({
    method: "PUT",
    path: `/v1/models/${encodeURIComponent(key)}`,
  }),
  proactivity: null,
  tools: null,
  automations: null,
  skills: null,
  colonizer: null,
  stack: null,
};

/** The model roles the server accepts (`livingbrain-models` `ROLES`). */
export const MODEL_ROLES = ["triage", "main", "research"];

/** True when the section is one the page knows about. */
export function isSection(section) {
  return SECTIONS.includes(section);
}

/**
 * Describes one setting change.
 *
 * Returns:
 *   `{section, key, value, wired, method, path, body}` where `wired: false`
 *   means `method` and `path` are null and the caller must not send anything.
 *
 * Throws on an unknown section or an empty key, because a change that cannot
 * be named cannot be audited, and a silent no-op in the settings page is worse
 * than a visible error.
 */
export function settingRequest(section, key, value) {
  if (!isSection(section)) {
    throw new Error(`unknown settings section: ${String(section)}`);
  }
  const name = String(key == null ? "" : key).trim();
  if (!name) {
    throw new Error("a settings change needs a key");
  }

  const endpoint = ENDPOINTS[section];
  const body = { section, key: name, value };

  if (!endpoint) {
    return {
      section,
      key: name,
      value,
      wired: false,
      method: null,
      path: null,
      body,
    };
  }

  const route = endpoint(name);
  // The models handler reads its fields flat (`provider`, `base_url`,
  // `api_key`, `model`, `fallback_to_managed`), so they are spread at the top
  // level next to the audit triple. Serde ignores the extra keys it does not
  // know, and `section`/`key`/`value` are what the audit reads.
  const payload =
    section === "models" && value && typeof value === "object" && !Array.isArray(value)
      ? { ...value, section, key: name, value }
      : body;

  return {
    section,
    key: name,
    value,
    wired: true,
    method: route.method,
    path: route.path,
    body: payload,
  };
}

/**
 * The audit line a server would write for a change. Exported so the shape is
 * pinned by a test rather than only implied by the request body.
 */
export function auditRecord(request) {
  return {
    section: request.section,
    key: request.key,
    value: request.value,
    method: request.method,
    path: request.path,
  };
}

/** Human-readable one-liners for the notice area, kept here so they test too. */
export function describeOutcome(request, ok) {
  const name = `${request.section}.${request.key}`;
  if (!request.wired) {
    return `${name} has no server route yet — nothing was sent.`;
  }
  return ok ? `${name} saved.` : `${name} could not be saved.`;
}

/* ------------------------------------------------------------ built with */

/** The vendored FZ-018 registry entry, for the page and for the tests. */
export const STACK = stack;

/**
 * The two statuses the registry uses, as words.
 *
 * Order is not significant here: `stackStatus` names `live` exactly and treats
 * everything else as `planned`, so a status the registry has never introduced
 * still lands in the *cautious* bucket.
 */
export const STACK_STATUSES = ["live", "planned"];

/** Reads an entry's status, defaulting to `planned` for anything unknown. */
export function stackStatus(entry) {
  return entry && entry.status === "live" ? "live" : "planned";
}

/**
 * The "Built with" rows, ready for the page to render.
 *
 * Ordering: what is in use today comes first, then what is planned. The
 * registry lists the one live entry *last* (hosting sits at the bottom of the
 * list it was cut from), which reads as "everything here is aspirational" — the
 * opposite of what is true. Within each group the registry's own order is
 * kept, so the reader still sees the list the registry published.
 *
 * The status word is taken from `stackStatus`, never from the entry's raw
 * field, so a planned entry cannot be rendered as live even if the data said
 * so: `data-state` and the word in the pill come from the same normalised
 * value.
 *
 * Pure: it takes the parsed document and answers plain data, so the tests call
 * it with `STACK` and the page calls it with the same object.
 */
export function stackEntries(data = stack) {
  const uses = Array.isArray(data && data.uses) ? data.uses : [];
  const statuses = (data && data.statuses) || {};
  // Groups in reading order: in use today, then planned. Each group keeps the
  // registry's own order within it.
  return ["live", "planned"].flatMap((status) =>
    uses
      .filter((entry) => stackStatus(entry) === status)
      .map((entry) => ({
        id: entry.id,
        name: entry.name,
        kind: entry.kind,
        url: entry.url,
        role: entry.role,
        phrase: entry.phrase,
        note: entry.note,
        status,
        // The word itself, and the registry's sentence for it, so "planned"
        // never reads as an unqualified "live".
        word: status,
        detail: statuses[status] || "",
      })),
  );
}

/**
 * The subprocessors link, or null when the document does not name one.
 *
 * The registry has no subprocessors concept: `subprocessors` is the venture's
 * own page, so the label says so instead of claiming a dedicated list that is
 * not published yet.
 */
export function stackSubprocessors(data = stack) {
  const url = data && data.subprocessors;
  return url
    ? {
        href: String(url),
        label: "Subprocessors (venture page; no privacy page yet)",
      }
    : null;
}

/**
 * Paints the "Built with" rows into a container.
 *
 * Elements and text nodes only — there is no `innerHTML` anywhere in `app/`,
 * so a registry entry can never become markup. Every href goes through
 * `isSafeHref`, so a re-vendored entry cannot smuggle a `javascript:` URL into
 * the page either.
 */
export function renderStackRows(container, data = stack) {
  if (!container) return container;
  container.replaceChildren();
  for (const entry of stackEntries(data)) {
    const row = document.createElement("p");
    row.className = "stack-row";

    const phrase = document.createElement("span");
    phrase.className = "muted";
    phrase.textContent = `${entry.phrase} `;
    row.appendChild(phrase);

    const link = document.createElement("a");
    link.textContent = entry.name;
    if (isSafeHref(entry.url)) {
      link.setAttribute("href", entry.url);
      link.setAttribute("rel", "noopener noreferrer");
    }
    row.appendChild(link);

    const pill = document.createElement("span");
    pill.className = "pill";
    // The one normalised value, used for both: the state and the word cannot
    // disagree, so a planned entry is never painted or labelled as live.
    pill.setAttribute("data-state", entry.status);
    pill.textContent = entry.detail;
    row.appendChild(document.createTextNode(" "));
    row.appendChild(pill);

    if (entry.note) {
      const note = document.createElement("span");
      note.className = "tiny";
      note.textContent = ` ${entry.note}`;
      row.appendChild(note);
    }
    container.appendChild(row);
  }
  return container;
}

/**
 * Fills the venture id and the two links from the vendored document, so the
 * URLs live in exactly one place (`stack.json`) instead of being hardcoded in
 * the HTML as well.
 *
 * Exported so the tests can assert the rendered hrefs against `STACK` without a
 * page: `bootStack` is only the DOM wrapper around this.
 */
export function renderStackLinks(doc = typeof document === "undefined" ? null : document, data = stack) {
  if (!doc) return null;
  const venture = doc.querySelector("#stack-venture");
  if (venture) venture.textContent = `${data.venture.id} (${data.venture.name})`;

  const list = stackSubprocessors(data);
  const sub = doc.querySelector("#stack-subprocessors");
  if (sub) {
    if (list) {
      sub.textContent = list.label;
      if (isSafeHref(list.href)) {
        sub.setAttribute("href", list.href);
        sub.setAttribute("rel", "noopener noreferrer");
      }
    }
  }

  const source = doc.querySelector("#stack-source");
  if (source && isSafeHref(data.source)) {
    source.setAttribute("href", String(data.source));
    source.setAttribute("rel", "noopener noreferrer");
  }
  return doc;
}

/** Boots the section on the settings page, and nowhere else. */
function bootStack() {
  const container = document.querySelector("#stack-rows");
  if (container) renderStackRows(container);
  renderStackLinks();
}

if (typeof document !== "undefined") {
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", bootStack);
  } else {
    bootStack();
  }
}