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

/** One History entry: the range, the value, when it was learned, its status.
 * A retracted fact is struck through, and says so in words too. */
function historyItem(fact, showSubject) {
  const label = showSubject ? `${fact.attribute} (${fact.subject})` : fact.attribute;
  const line = [
    el("strong", [tx(displayRange(fact))]),
    tx(` · ${label} = ${displayValue(fact)} · observed ${observedDay(fact)}`),
  ];
  const status = fact.status || "current";
  if (status === "retracted") {
    return el("li", [el("s", line), tx(" · retracted")], { class: "facts__retracted" });
  }
  const word = status === "superseded" && fact.superseded_by ? "superseded" : status;
  return el("li", [...line, tx(" · "), el("span", [tx(word)], { class: `pill facts__${status}` })]);
}

/**
 * The facts section for a set of timelines (`keys`): a Current table — the
 * facts valid at the asked time, one row each — and a History list, newest
 * first. `showSubject` adds the subject to each row, for the Facts view where
 * several entities share one list. An empty or missing `keys` is `[]`, so a
 * caller can hide the card.
 */
export function factsDescription(keys, { showSubject = false } = {}) {
  const list = Array.isArray(keys) ? keys.filter((key) => key && key.attribute) : [];
  if (list.length === 0) return [];
  const rows = [];
  const history = [];
  for (const key of list) {
    for (const fact of Array.isArray(key.current) ? key.current : []) {
      const cells = [
        el("td", [tx(showSubject ? `${key.attribute} (${key.subject})` : key.attribute)]),
        el("td", [tx(displayValue(fact))]),
        el("td", [tx(displayRange(fact))]),
        el("td", [tx(observedDay(fact))]),
      ];
      if (fact.status === "disputed") {
        cells[1].children.push(tx(" "), el("span", [tx("disputed")], { class: "pill facts__disputed" }));
      }
      rows.push(el("tr", cells));
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
            el("tr", ["Attribute", "Value", "Valid", "Observed"].map((h) => el("th", [tx(h)]))),
          ]),
          el("tbody", rows),
        ],
        { class: "facts__table" },
      ),
    );
  } else {
    out.push(el("p", [tx("Nothing was valid at that time.")], { class: "tiny" }));
  }
  out.push(el("h4", [tx("History")]));
  out.push(el("ul", history.map((fact) => historyItem(fact, showSubject)), { class: "facts__history" }));
  return out;
}
