// Temporal facts (docs/temporal-facts.md) for the wiki page: what is true
// about an entity now (or as of a date), and the history of what was true
// before. Pure description builders — every node goes through `renderInto`
// / `mountNodes` as `{type, tag, children, attrs}` descriptions with text in
// `textContent`, so nothing here builds HTML from strings (no `innerHTML`).
//
// The server decides what is current and what was superseded; this file only
// lays the answer out: `GET /v1/facts` and the page view's `facts` both carry
// `keys`, each `{subject, attribute, current: [fact], history: [fact]}`.

/** A partial date the date picker can send: `YYYY-MM-DD`, or "" for now. */
const DATE_PATTERN = /^\d{4}(-\d{2}(-\d{2})?)?$/;

/** A fact not confirmed in this many days is stale: hidden unless asked for. */
export const STALE_DAYS = 90;

const DAY_MS = 86400000;

function tx(value) {
  return { type: "text", value: String(value) };
}

function el(tag, children, attrs) {
  const node = { type: "element", tag, children: children || [] };
  if (attrs && Object.keys(attrs).length) node.attrs = attrs;
  return node;
}

/** `GET /v1/facts` for a subject and/or attribute, as of a date ("" = now). */
export function factsPath({ subject = "", attribute = "", asOf = "" } = {}) {
  const params = new URLSearchParams();
  if (subject) params.set("subject", subject);
  if (attribute) params.set("attribute", attribute);
  if (asOf && DATE_PATTERN.test(asOf)) params.set("as_of", asOf);
  const query = params.toString();
  return query ? `/v1/facts?${query}` : "/v1/facts";
}

/** `41 EU`, or `41` when the fact has no unit. */
export function displayValue(fact) {
  const value = fact && fact.value != null ? String(fact.value) : "";
  return fact && fact.unit ? `${value} ${fact.unit}` : value;
}

/** `2025 – 2026`, or `2026 – now` while it still holds. */
export function displayRange(fact) {
  const from = (fact && fact.valid_from) || "?";
  const to = (fact && fact.valid_to) || "now";
  return `${from} – ${to}`;
}

/** The day an observation was made: the date part of its RFC 3339 stamp. */
function observedDay(fact) {
  return String((fact && fact.observed_at) || "").slice(0, 10);
}

/** The day a fact was last confirmed (`YYYY-MM-DD`): when a write last said
 * it again, else when it was observed. */
export function confirmedDay(fact) {
  return String((fact && (fact.last_confirmed_at || fact.observed_at)) || "").slice(0, 10);
}

/** Whether a fact was last confirmed more than `STALE_DAYS` before `now`
 * (milliseconds). A fact with no readable date is not stale: hiding what
 * cannot be dated would hide the whole list. */
export function isStale(fact, now = Date.now()) {
  const day = confirmedDay(fact);
  if (!/^\d{4}-\d{2}-\d{2}$/.test(day)) return false;
  return now - Date.parse(`${day}T00:00:00Z`) > STALE_DAYS * DAY_MS;
}

/** How many current facts in `keys` are stale at `now`: what "Show stale"
 * would reveal. */
export function staleCount(keys, now = Date.now()) {
  let count = 0;
  for (const key of Array.isArray(keys) ? keys : []) {
    for (const fact of Array.isArray(key && key.current) ? key.current : []) {
      if (isStale(fact, now)) count += 1;
    }
  }
  return count;
}

/** `today`, `yesterday`, or `N days ago` for a `YYYY-MM-DD` day, against
 * `now` (milliseconds). */
export function daysAgoLabel(day, now = Date.now()) {
  const at = Date.parse(`${String(day).slice(0, 10)}T00:00:00Z`);
  if (Number.isNaN(at)) return "";
  // Whole days elapsed since that day's midnight (UTC): a Tuesday's
  // evening is `yesterday` on Wednesday, not `today`.
  const days = Math.max(0, Math.floor((now - at) / DAY_MS));
  if (days === 0) return "today";
  if (days === 1) return "yesterday";
  return `${days} days ago`;
}

/** `Last dreamt 2 days ago · 5 facts updated` for a timeline's last dream
 * pass (`key.dream`, from the read route), or `null` when it was never
 * dreamt about. */
export function dreamSummary(key, now = Date.now()) {
  const dream = key && key.dream;
  if (!dream || !dream.at) return null;
  const updated = Number(dream.applied) || 0;
  const ago = daysAgoLabel(dream.at, now);
  const facts = `${updated} fact${updated === 1 ? "" : "s"} updated`;
  return `Last dreamt ${ago} · ${facts}`;
}

/** The label for a fact a dream changed (`fact.dreamed`, the decision's op),
 * or `null` for a fact no dream touched. */
export function dreamedLabel(fact) {
  const op = fact && fact.dreamed;
  if (op === "retract" || op === "end") return "Ended while dreaming";
  if (op === "supersede") return "Superseded while dreaming";
  if (op === "rename") return "Renamed while dreaming";
  return null;
}

/** One History entry: the range, the value, when it was learned, its status.
 * A retracted fact is struck through, and says so in words too. */
function historyItem(fact, showSubject) {
  const label = showSubject ? `${fact.attribute} (${fact.subject})` : fact.attribute;
  const line = [
    el("strong", [tx(displayRange(fact))]),
    tx(` · ${label} = ${displayValue(fact)} · observed ${observedDay(fact)}`),
  ];
  const status = fact.status || "current";
  const dreamed = dreamedLabel(fact);
  if (status === "retracted") {
    const note = dreamed ? ` · ${dreamed}` : " · retracted";
    return el("li", [el("s", line), tx(note)], { class: "facts__retracted" });
  }
  const word = status === "superseded" && fact.superseded_by ? "superseded" : status;
  const tail = [tx(" · "), el("span", [tx(word)], { class: `pill facts__${status}` })];
  if (dreamed) tail.push(tx(" · "), el("span", [tx(dreamed)], { class: "pill facts__dreamed" }));
  return el("li", [...line, ...tail]);
}

/**
 * The facts section for a set of timelines (`keys`): a Current table — the
 * facts valid at the asked time, one row each — and a History list, newest
 * first. `showSubject` adds the subject to each row, for the Facts view where
 * several entities share one list. `showStale: false` leaves out the current
 * facts not confirmed within `STALE_DAYS` (of `now`); the history always
 * shows everything, since it is what was true before. Each timeline a dream
 * has tidied says when, under the table. An empty or missing `keys` is `[]`,
 * so a caller can hide the card.
 */
export function factsDescription(
  keys,
  { showSubject = false, showStale = true, now = Date.now() } = {},
) {
  const list = Array.isArray(keys) ? keys.filter((key) => key && key.attribute) : [];
  if (list.length === 0) return [];
  const rows = [];
  const history = [];
  const dreams = [];
  for (const key of list) {
    for (const fact of Array.isArray(key.current) ? key.current : []) {
      if (!showStale && isStale(fact, now)) continue;
      const cells = [
        el("td", [tx(showSubject ? `${key.attribute} (${key.subject})` : key.attribute)]),
        el("td", [tx(displayValue(fact))]),
        el("td", [tx(displayRange(fact))]),
        el("td", [tx(observedDay(fact))]),
        el("td", [tx(confirmedDay(fact))]),
      ];
      if (fact.status === "disputed") {
        cells[1].children.push(tx(" "), el("span", [tx("disputed")], { class: "pill facts__disputed" }));
      }
      const dreamed = dreamedLabel(fact);
      if (dreamed) {
        cells[1].children.push(tx(" "), el("span", [tx(dreamed)], { class: "pill facts__dreamed" }));
      }
      rows.push(el("tr", cells));
    }
    const summary = dreamSummary(key, now);
    if (summary) {
      dreams.push(el("p", [tx(`${key.subject} · ${summary}`)], { class: "tiny facts__dream" }));
    }
    // The key names the timeline; a fact row may not repeat it.
    for (const fact of Array.isArray(key.history) ? key.history : []) {
      history.push({ subject: key.subject, attribute: key.attribute, ...fact });
    }
  }
  history.sort((a, b) =>
    String(b.valid_from || "").localeCompare(String(a.valid_from || "")) ||
    String(b.observed_at || "").localeCompare(String(a.observed_at || "")),
  );
  const out = [el("h4", [tx("Current")])];
  if (rows.length) {
    out.push(
      el(
        "table",
        [
          el("thead", [
            el("tr", ["Attribute", "Value", "Valid", "Observed", "Confirmed"].map((h) => el("th", [tx(h)]))),
          ]),
          el("tbody", rows),
        ],
        { class: "facts__table" },
      ),
    );
  } else {
    out.push(el("p", [tx("Nothing was valid at that time.")], { class: "tiny" }));
  }
  out.push(...dreams);
  out.push(el("h4", [tx("History")]));
  out.push(el("ul", history.map((fact) => historyItem(fact, showSubject)), { class: "facts__history" }));
  return out;
}

/* -------------------------------------------------------------------------
 * The Facts view (`wiki.html?view=facts`): one readable card per attribute,
 * grouped by subject. The page card above keeps its table; these helpers
 * only shape the view, so every rule here is a pure function a test can
 * call with a fixed `now`.
 * ------------------------------------------------------------------------- */

/** Subjects show this many facts; the rest wait behind "Show all". */
export const LIST_LIMIT = 50;

/** A value longer than this is clamped to three lines with "Show more". */
const LONG_VALUE = 160;

/** `auth_provider_production` -> `Auth provider production`. An all-caps
 * word of two or more letters (`API`) keeps its capitals. */
export function humanAttribute(key) {
  const words = String(key || "").split(/[_\s-]+/).filter(Boolean);
  const text = words
    .map((word) => (word.length > 1 && word === word.toUpperCase() && /[A-Z]/.test(word) ? word : word.toLowerCase()))
    .join(" ");
  return text.charAt(0).toUpperCase() + text.slice(1);
}

/** `project/earthos-world` -> `{ type: "project", name: "earthos-world" }`. A
 * subject with no type keeps its whole text as the name. */
export function subjectParts(subject) {
  const text = String(subject || "");
  const slash = text.indexOf("/");
  if (slash < 0) return { type: "", name: text };
  return { type: text.slice(0, slash), name: text.slice(slash + 1) };
}

/** A partial date (`2026`, `2026-10`, `2026-10-02`) in the reader's locale,
 * e.g. `Oct 2, 2026`. `withYear: false` drops the year. Empty in, empty out. */
export function formatDay(day, { withYear = true } = {}) {
  const match = /^(\d{4})(?:-(\d{2})(?:-(\d{2}))?)?$/.exec(String(day || "").slice(0, 10));
  if (!match) return "";
  const [, year, month, date] = match;
  if (!month) return year;
  const options = date ? { month: "short", day: "numeric" } : { month: "short" };
  if (withYear) options.year = "numeric";
  const at = Date.UTC(Number(year), Number(month) - 1, Number(date || 1));
  return new Intl.DateTimeFormat(undefined, { ...options, timeZone: "UTC" }).format(at);
}

/** `Since Oct 2, 2026` while a fact holds; `Oct 2 – Oct 9, 2026` once it
 * ended (the year is written once when both ends share it). */
export function factSince(fact) {
  const from = (fact && fact.valid_from) || "";
  const to = (fact && fact.valid_to) || "";
  if (!to) return from ? `Since ${formatDay(from)}` : "Current";
  if (!from) return `Until ${formatDay(to)}`;
  const sameYear = from.length === 10 && to.length === 10 && from.slice(0, 4) === to.slice(0, 4);
  const start = sameYear ? formatDay(from, { withYear: false }) : formatDay(from);
  return `${start} – ${formatDay(to)}`;
}

/** The meta line for a fact: its range, and `Confirmed …` only when the last
 * confirmation falls on a different day from the observation. */
export function factMeta(fact) {
  const observed = observedDay(fact);
  const confirmed = confirmedDay(fact);
  const differs = /^\d{4}-\d{2}-\d{2}$/.test(confirmed) && confirmed !== observed;
  return {
    since: factSince(fact),
    confirmed: differs ? `Confirmed ${formatDay(confirmed)}` : null,
    observed: formatDay(observed),
  };
}

/** The line under a subject heading, from the most recent dream of it. */
function subjectDreamLine(dream, now) {
  return dream ? dreamSummary({ dream }, now) : null;
}

/** Whether a search token list matches a text (every token, case-blind). */
function matchesAll(tokens, text) {
  const haystack = String(text || "").toLowerCase();
  return tokens.every((token) => haystack.includes(token));
}

/** Folds the timelines into one item per subject + attribute, with the current
 * facts the reader may see, the history beside them, and whether the search
 * matches. Items are grouped by subject, sorted, and counted. `alpha` sorts
 * by the humanised attribute; `recent` by when a value was last observed. */
export function buildFactsView(
  keys,
  { showStale = false, now = Date.now(), query = "", sort = "alpha" } = {},
) {
  const list = Array.isArray(keys) ? keys.filter((key) => key && key.attribute) : [];
  const tokens = String(query || "").toLowerCase().split(/\s+/).filter(Boolean);
  const merged = new Map();
  const subjectDreams = new Map();
  for (const key of list) {
    const id = `${key.subject}\u0000${key.attribute}`;
    if (!merged.has(id)) {
      merged.set(id, { subject: String(key.subject || ""), attribute: key.attribute, current: [], history: [] });
    }
    const entry = merged.get(id);
    entry.current.push(...(Array.isArray(key.current) ? key.current : []));
    entry.history.push(...(Array.isArray(key.history) ? key.history : []));
    const dream = key.dream && key.dream.at ? key.dream : null;
    const latest = subjectDreams.get(entry.subject);
    if (dream && (!latest || String(dream.at) > String(latest.at))) subjectDreams.set(entry.subject, dream);
  }

  const subjects = new Map();
  for (const entry of merged.values()) {
    const currentIds = new Set(entry.current.map((fact) => fact.id));
    const current = entry.current
      .filter((fact) => showStale || !isStale(fact, now))
      .sort((a, b) => String(b.valid_from || "").localeCompare(String(a.valid_from || "")));
    const seen = new Set();
    const history = entry.history
      .filter((fact) => {
        if (currentIds.has(fact.id) || seen.has(fact.id)) return false;
        seen.add(fact.id);
        return true;
      })
      .sort((a, b) =>
        String(b.valid_from || "").localeCompare(String(a.valid_from || "")) ||
        String(b.observed_at || "").localeCompare(String(a.observed_at || "")),
      );
    if (current.length === 0 && history.length === 0) continue;

    const title = humanAttribute(entry.attribute);
    const attributeMatch = tokens.length === 0 || matchesAll(tokens, `${entry.attribute} ${title}`);
    const valueMatch = (fact) => matchesAll(tokens, displayValue(fact));
    const matched = attributeMatch ? current.length : current.filter(valueMatch).length;
    if (!attributeMatch && matched === 0 && !history.some(valueMatch)) continue;

    const latest = current.reduce(
      (best, fact) => {
        const day = observedDay(fact) || String(fact.valid_from || "");
        return day > best ? day : best;
      },
      "",
    );
    const dreamed = current.map(dreamedLabel).find(Boolean) || null;
    const item = {
      id: `${entry.subject}\u0000${entry.attribute}`,
      subject: entry.subject,
      attribute: entry.attribute,
      title,
      current,
      history,
      matched,
      latest,
      hiddenStale: entry.current.length - current.length,
      disputed: current.some((fact) => fact.status === "disputed"),
      stale: current.some((fact) => isStale(fact, now)),
      dreamed,
    };
    if (!subjects.has(entry.subject)) {
      const parts = subjectParts(entry.subject);
      subjects.set(entry.subject, {
        subject: entry.subject,
        type: parts.type,
        name: parts.name,
        count: 0,
        dream: subjectDreams.get(entry.subject) || null,
        items: [],
      });
    }
    const group = subjects.get(entry.subject);
    group.items.push(item);
    group.count += matched;
  }

  const byTitle = (a, b) => a.title.localeCompare(b.title, undefined, { sensitivity: "base" });
  for (const group of subjects.values()) {
    group.items.sort(
      sort === "recent"
        ? (a, b) => (a.latest === b.latest ? byTitle(a, b) : a.latest < b.latest ? 1 : -1)
        : byTitle,
    );
  }
  const ordered = [...subjects.values()].sort(
    (a, b) => b.count - a.count || a.name.localeCompare(b.name, undefined, { sensitivity: "base" }),
  );
  return {
    subjects: ordered,
    matched: ordered.reduce((sum, group) => sum + group.count, 0),
  };
}

/** The subject rail (desktop) and the subject select (mobile) as one list of
 * jump targets: each names its section, its friendly name, type and count. */
export function factsSubjectLinks(view) {
  return view.subjects.map((group, index) => ({
    target: `facts-subject-${index}`,
    name: group.name,
    type: group.type,
    count: group.count,
    label: `${group.name} · ${group.count}`,
  }));
}

function factValueNode(fact) {
  return el("span", [tx(displayValue(fact))], {
    class: fact.status === "retracted" ? "fact__value fact__value--retracted" : "fact__value",
  });
}

/** The meta spans of one fact, each with its exact date on hover. */
function factMetaNodes(fact) {
  const meta = factMeta(fact);
  const nodes = [el("span", [tx(meta.since)], { class: "fact__since", title: displayRange(fact) })];
  if (meta.confirmed) {
    nodes.push(el("span", [tx(meta.confirmed)], { class: "fact__confirmed", title: confirmedDay(fact) }));
  }
  return nodes;
}

function joinSpans(spans) {
  const out = [];
  spans.forEach((span, index) => {
    if (index) out.push(tx(" · "));
    out.push(span);
  });
  return out;
}

function historyNode(fact) {
  const status = fact.status || "current";
  const retracted = status === "retracted";
  const word = status === "superseded" && fact.superseded_by ? "superseded" : status;
  const parts = [
    el("span", [tx(displayRange(fact))], { class: "fact__hist-range" }),
    el("span", [tx(displayValue(fact))], { class: retracted ? "fact__hist-value fact__value--retracted" : "fact__hist-value" }),
    el("span", [tx(word)], { class: `pill facts__${status}` }),
  ];
  const dreamed = dreamedLabel(fact);
  if (dreamed) parts.push(el("span", [tx(dreamed)], { class: "pill facts__dreamed" }));
  return el("li", joinSpans(parts));
}

/** One attribute as a row: humanised title, its value(s), the meta line, and
 * the raw key and history folded away behind disclosures. */
function factItemNode(item) {
  const badges = [];
  if (item.disputed) badges.push(el("span", [tx("Disputed")], { class: "pill fact__badge fact__badge--disputed" }));
  if (item.stale) badges.push(el("span", [tx("Stale")], { class: "pill fact__badge fact__badge--stale" }));
  if (item.dreamed) badges.push(el("span", [tx(item.dreamed)], { class: "pill fact__badge fact__badge--dreamed" }));
  const body = [el("div", [el("h3", [tx(item.title)], { class: "fact__title", title: item.attribute }), ...badges], { class: "fact__head" })];
  // The meta line, the raw key and the history share one line of small type.
  const foot = [];

  if (item.current.length === 0) {
    const words = item.hiddenStale
      ? "Not confirmed lately, so hidden. Turn on Show stale to see it."
      : "Nothing is current at this date.";
    body.push(el("p", [tx(words)], { class: "fact__none" }));
  } else if (item.current.length === 1) {
    const [fact] = item.current;
    const text = displayValue(fact);
    const long = text.length > LONG_VALUE || text.includes("\n");
    body.push(el("p", [tx(text)], { class: long ? "fact__value fact__value--clamp" : "fact__value" }));
    if (long) {
      body.push(el("button", [tx("Show more")], { type: "button", class: "fact__more", "aria-expanded": "false", "data-more": "" }));
    }
    foot.push(el("p", joinSpans(factMetaNodes(fact)), { class: "fact__meta" }));
  } else {
    if (item.disputed) body.push(el("p", [tx("Competing values")], { class: "fact__lead" }));
    body.push(
      el(
        "ul",
        item.current.map((fact) =>
          el("li", [factValueNode(fact), el("span", joinSpans(factMetaNodes(fact)), { class: "fact__meta" })], {
            class: "fact__option",
          }),
        ),
        { class: "fact__options" },
      ),
    );
  }

  const details = [
    el("summary", [tx("Details")]),
    el("dl", [
      el("dt", [tx("Attribute key")]),
      el("dd", [el("code", [tx(item.attribute)])]),
      el("dt", [tx("Observed")]),
      el("dd", [tx(item.current.map((fact) => formatDay(observedDay(fact))).filter(Boolean).join(" · ") || "unknown")]),
    ]),
  ];
  foot.push(el("details", details, { class: "fact__details" }));
  if (item.history.length) {
    foot.push(
      el(
        "details",
        [
          el("summary", [tx(`History (${item.history.length})`)]),
          el("ul", item.history.map(historyNode), { class: "fact__history" }),
        ],
        { class: "fact__past" },
      ),
    );
  }
  body.push(el("div", foot, { class: "fact__foot" }));
  const classes = ["fact"];
  if (item.disputed) classes.push("fact--disputed");
  if (item.stale) classes.push("fact--stale");
  return el("article", body, { class: classes.join(" ") });
}

/** The subject sections: a heading per subject with its count, the type and
 * the last dream, then its facts. A subject past `limit` shows "Show all N",
 * unless the reader has already expanded it (`expanded` holds subject keys). */
export function factsSectionsDescription(view, { expanded = new Set(), limit = LIST_LIMIT, now = Date.now() } = {}) {
  return view.subjects.map((group, index) => {
    const open = expanded.has(group.subject);
    const shown = open ? group.items : group.items.slice(0, limit);
    const head = [el("h2", [tx(group.name)], { class: "facts-subject__name" })];
    if (group.type) head.push(el("span", [tx(group.type)], { class: "facts-subject__type" }));
    head.push(el("span", [tx(`${group.count} ${group.count === 1 ? "fact" : "facts"}`)], { class: "facts-subject__count" }));
    const children = [el("header", [el("div", head, { class: "facts-subject__head" })])];
    const dream = subjectDreamLine(group.dream, now);
    if (dream) children.push(el("p", [tx(dream)], { class: "tiny facts-subject__dream" }));
    children.push(el("div", shown.map(factItemNode), { class: "facts-items" }));
    if (!open && group.items.length > limit) {
      children.push(
        el("button", [tx(`Show all ${group.items.length}`)], {
          type: "button",
          class: "facts-more",
          "data-expand": group.subject,
        }),
      );
    }
    return el("section", children, { class: "facts-subject", id: `facts-subject-${index}` });
  });
}

/** The subject rail: one button per subject, jumping to its section. */
export function factsRailDescription(view) {
  return factsSubjectLinks(view).map((link) =>
    el("li", [
      el("button", [
        el("span", [tx(link.name)], { class: "facts-rail__name" }),
        el("span", [tx([link.type, link.count].filter((part) => part !== "").join(" · "))], { class: "facts-rail__meta" }),
      ], { type: "button", class: "facts-rail__item", "data-jump": link.target }),
    ]),
  );
}

/** The subject select for narrow screens: one option per subject. */
export function factsSelectDescription(view) {
  return factsSubjectLinks(view).map((link) =>
    el("option", [tx(link.label)], { value: link.target }),
  );
}

/** The line that says what the view shows: the count, the as-of date, and
 * how many stale facts are hidden. */
export function factsStatusText({ matched = 0, asOf = "", staleHidden = 0 } = {}) {
  const parts = [`${matched} ${matched === 1 ? "fact" : "facts"}`];
  if (asOf) parts.push(`as of ${formatDay(asOf)}`);
  if (staleHidden) parts.push(`${staleHidden} stale ${staleHidden === 1 ? "fact" : "facts"} hidden`);
  return parts.join(" · ");
}

/** What the list shows when no subject has a fact to show: a search that
 * matches nothing (with a way out), or nothing current at that date. */
export function factsNoMatchDescription(query = "") {
  if (!query) {
    return [
      el("p", [tx("Nothing is current at this date. Turn on Show stale to see facts nobody has confirmed lately.")], {
        class: "facts-none",
      }),
    ];
  }
  return [
    el("p", [tx(`No facts match “${query}”.`)], { class: "facts-none" }),
    el("button", [tx("Clear search")], { type: "button", class: "facts-more", "data-clear": "" }),
  ];
}
