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
