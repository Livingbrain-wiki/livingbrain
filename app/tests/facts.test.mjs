// The temporal facts helpers (assets/facts.js): freshness, the stale filter,
// the dream line and the labels for facts a dream changed. Pure functions, so
// the tests import them directly and pass `now` explicitly.

import { test } from "node:test";
import assert from "node:assert/strict";

import {
  STALE_DAYS,
  confirmedDay,
  daysAgoLabel,
  dreamSummary,
  dreamedLabel,
  factsDescription,
  isStale,
  staleCount,
} from "../assets/facts.js";

const NOW = Date.parse("2026-10-16T12:00:00Z");

function fact(overrides = {}) {
  return {
    id: "fact-1",
    value: "D1",
    unit: null,
    valid_from: "2026-01",
    valid_to: null,
    observed_at: "2026-01-05T09:00:00Z",
    last_confirmed_at: "2026-01-05T09:00:00Z",
    status: "current",
    ...overrides,
  };
}

function text(nodes) {
  // Flattens a description into its text, for assertions on what is shown.
  const out = [];
  const walk = (node) => {
    if (!node) return;
    if (node.type === "text") out.push(node.value);
    for (const child of node.children || []) walk(child);
  };
  nodes.forEach(walk);
  return out.join("");
}

test("a fact is stale once it has gone STALE_DAYS without a confirmation", () => {
  assert.equal(STALE_DAYS, 90);
  const day = (n) => new Date(NOW - n * 86400000).toISOString().slice(0, 10);
  assert.equal(isStale(fact({ last_confirmed_at: `${day(89)}T00:00:00Z` }), NOW), false);
  assert.equal(isStale(fact({ last_confirmed_at: `${day(91)}T00:00:00Z` }), NOW), true);
});

test("freshness is the last confirmation, falling back to the observation", () => {
  assert.equal(confirmedDay(fact({ last_confirmed_at: "2026-10-01T08:00:00Z" })), "2026-10-01");
  const legacy = fact({ last_confirmed_at: undefined, observed_at: "2025-02-01T00:00:00Z" });
  assert.equal(confirmedDay(legacy), "2025-02-01");
  assert.equal(isStale(legacy, NOW), true);
  assert.equal(isStale({ value: "x" }, NOW), false, "an undated fact is not hidden");
});

test("staleCount counts the current facts a view would hide", () => {
  const keys = [
    {
      subject: "project/demo",
      attribute: "database",
      current: [fact({ id: "a" }), fact({ id: "b", last_confirmed_at: "2026-10-10T00:00:00Z" })],
      history: [],
    },
  ];
  assert.equal(staleCount(keys, NOW), 1);
  assert.equal(staleCount([], NOW), 0);
});

test("the Facts description leaves stale current facts out unless asked, and keeps history", () => {
  const keys = [
    {
      subject: "project/demo",
      attribute: "database",
      current: [fact({ id: "old", value: "Postgres", last_confirmed_at: "2025-01-01T00:00:00Z" })],
      history: [fact({ id: "old", value: "Postgres", last_confirmed_at: "2025-01-01T00:00:00Z" })],
    },
  ];
  const hidden = text(factsDescription(keys, { showStale: false, now: NOW }));
  assert.match(hidden, /Nothing was valid at that time/u, "the current row is hidden");
  assert.match(hidden, /database = Postgres/u, "the history still says what was true");
  const shown = text(factsDescription(keys, { showStale: true, now: NOW }));
  assert.match(shown, /database = Postgres/u);
});

test("the dream line says when a timeline was last tidied and what changed", () => {
  assert.equal(daysAgoLabel("2026-10-14", NOW), "2 days ago");
  assert.equal(daysAgoLabel("2026-10-16", NOW), "today");
  assert.equal(daysAgoLabel("2026-10-15", NOW), "yesterday");
  assert.equal(
    dreamSummary({ dream: { at: "2026-10-14T03:00:00Z", applied: 5 } }, NOW),
    "Last dreamt 2 days ago · 5 facts updated",
  );
  assert.equal(
    dreamSummary({ dream: { at: "2026-10-16T03:00:00Z", applied: 1 } }, NOW),
    "Last dreamt today · 1 fact updated",
  );
  assert.equal(dreamSummary({}, NOW), null, "never dreamt: no line");
  const described = text(
    factsDescription(
      [
        {
          subject: "project/demo",
          attribute: "database",
          current: [fact()],
          history: [],
          dream: { at: "2026-10-14T03:00:00Z", applied: 5 },
        },
      ],
      { now: NOW },
    ),
  );
  assert.match(described, /project\/demo · Last dreamt 2 days ago · 5 facts updated/u);
});

test("a fact a dream ended, superseded or renamed says so in words", () => {
  assert.equal(dreamedLabel({ dreamed: "retract" }), "Ended while dreaming");
  assert.equal(dreamedLabel({ dreamed: "end" }), "Ended while dreaming");
  assert.equal(dreamedLabel({ dreamed: "supersede" }), "Superseded while dreaming");
  assert.equal(dreamedLabel({ dreamed: "rename" }), "Renamed while dreaming");
  assert.equal(dreamedLabel({}), null);
  const described = text(
    factsDescription(
      [
        {
          subject: "project/demo",
          attribute: "session_cost",
          current: [],
          history: [fact({ status: "retracted", dreamed: "retract" })],
        },
      ],
      { now: NOW },
    ),
  );
  assert.match(described, /Ended while dreaming/u);
});

/* ------------------------------------------------ the Facts view (grouped) */

import {
  LIST_LIMIT,
  buildFactsView,
  factMeta,
  factSince,
  factsRailDescription,
  factsSectionsDescription,
  factsStatusText,
  formatDay,
  humanAttribute,
  subjectParts,
} from "../assets/facts.js";

/** A fact observed and confirmed in October 2026, so fresh against NOW. */
function fresh(overrides = {}) {
  return fact({ observed_at: "2026-10-10T09:00:00Z", last_confirmed_at: "2026-10-10T09:00:00Z", valid_from: "2026-10", ...overrides });
}

function key(subject, attribute, current, history = current, extra = {}) {
  return { subject, attribute, scope: "shared", cardinality: "one", current, history, ...extra };
}

/** The text of every element with a class in a description, in order. */
function classText(nodes, className) {
  const out = [];
  const walk = (node) => {
    if (!node || node.type !== "element") return;
    if (node.attrs && String(node.attrs.class || "").split(/\s+/).includes(className)) {
      out.push(text([node]));
    }
    for (const child of node.children || []) walk(child);
  };
  nodes.forEach(walk);
  return out;
}

test("humanAttribute turns a snake_case key into a sentence-case title", () => {
  assert.equal(humanAttribute("auth_provider_production"), "Auth provider production");
  assert.equal(humanAttribute("shoe_size"), "Shoe size");
  assert.equal(humanAttribute("API_key"), "API key", "an all-caps word keeps its capitals");
  assert.equal(humanAttribute("cdn-provider"), "Cdn provider");
  assert.equal(humanAttribute(""), "");
});

test("subjectParts splits the type from the friendly name", () => {
  assert.deepEqual(subjectParts("project/earthos-world"), { type: "project", name: "earthos-world" });
  assert.deepEqual(subjectParts("person/nick"), { type: "person", name: "nick" });
  assert.deepEqual(subjectParts("loose"), { type: "", name: "loose" });
});

test("the meta line: since while it holds, a range once it ended, confirmed only when it differs", () => {
  const holds = fresh({ valid_from: "2026-10-02", observed_at: "2026-10-02T09:00:00Z", last_confirmed_at: "2026-10-02T09:00:00Z" });
  assert.equal(factSince(holds), `Since ${formatDay("2026-10-02")}`);
  assert.equal(factMeta(holds).confirmed, null, "confirmed the same day it was observed: not shown");

  const reconfirmed = fresh({ valid_from: "2026-10-02", observed_at: "2026-10-02T09:00:00Z", last_confirmed_at: "2026-10-15T08:00:00Z" });
  assert.equal(factMeta(reconfirmed).confirmed, `Confirmed ${formatDay("2026-10-15")}`);

  const ended = fresh({ valid_from: "2026-10-02", valid_to: "2026-10-09" });
  assert.equal(factSince(ended), `${formatDay("2026-10-02", { withYear: false })} – ${formatDay("2026-10-09")}`);
  assert.equal(factSince({ valid_from: "2025", valid_to: "2026" }), `${formatDay("2025")} – ${formatDay("2026")}`);
  assert.match(formatDay("2026-10"), /2026/u, "a month keeps its year");
  assert.equal(formatDay(""), "");
});

test("subjects group their facts, carry counts, and sort by size", () => {
  const keys = [
    key("project/earthos-world", "auth_provider_production", [fresh({ id: "a" })]),
    key("project/earthos-world", "database_engine", [fresh({ id: "b" })]),
    key("project/colonizer", "runtime", [fresh({ id: "c" })]),
    key("person/nick", "shoe_size", [fresh({ id: "d" })]),
  ];
  const view = buildFactsView(keys, { now: NOW });
  assert.deepEqual(
    view.subjects.map((s) => [s.name, s.type, s.count]),
    [
      ["earthos-world", "project", 2],
      ["colonizer", "project", 1],
      ["nick", "person", 1],
    ],
  );
  assert.equal(view.matched, 4);
  const links = factsRailDescription(view).map((node) => text([node]));
  assert.match(links[0], /earthos-world.*project · 2/u);
  const sections = factsSectionsDescription(view, { now: NOW });
  assert.equal(sections[0].attrs.id, "facts-subject-0");
  assert.deepEqual(classText(sections, "facts-subject__count"), ["2 facts", "1 fact", "1 fact"]);
});

test("a subject's facts are alphabetical by title, or most recent first", () => {
  const keys = [
    key("project/demo", "zeta_mode", [fresh({ id: "z", observed_at: "2026-01-01T00:00:00Z" })]),
    key("project/demo", "alpha_mode", [fresh({ id: "a", observed_at: "2026-03-01T00:00:00Z" })]),
  ];
  const alpha = buildFactsView(keys, { now: NOW }).subjects[0].items.map((i) => i.title);
  assert.deepEqual(alpha, ["Alpha mode", "Zeta mode"]);
  const recent = buildFactsView(keys, { now: NOW, sort: "recent" }).subjects[0].items.map((i) => i.title);
  assert.deepEqual(recent, ["Alpha mode", "Zeta mode"], "a March observation is more recent than January's");
  const older = buildFactsView(
    [key("project/demo", "zeta_mode", [fresh({ id: "z", observed_at: "2026-05-01T00:00:00Z" })]), keys[1]],
    { now: NOW, sort: "recent" },
  ).subjects[0].items.map((i) => i.title);
  assert.deepEqual(older, ["Zeta mode", "Alpha mode"]);
});

test("two disputed versions of one attribute are one item with both values listed", () => {
  const a = fresh({ id: "a", value: "Postgres", status: "disputed" });
  const b = fresh({ id: "b", value: "MySQL", status: "disputed" });
  const view = buildFactsView([key("system/db", "engine", [a, b])], { now: NOW });
  const [item] = view.subjects[0].items;
  assert.equal(item.disputed, true);
  assert.equal(view.matched, 2);
  const sections = factsSectionsDescription(view, { now: NOW });
  assert.equal(classText(sections, "fact--disputed").length, 1);
  assert.deepEqual(classText(sections, "fact__badge--disputed"), ["Disputed"]);
  assert.deepEqual(classText(sections, "fact__option").map((t) => t.replace(/Since.*$/u, "").trim()), ["Postgres", "MySQL"]);
  assert.deepEqual(classText(sections, "fact__lead"), ["Competing values"]);
});

test("search matches attribute names and values, and counts the facts it finds", () => {
  const keys = [
    key("project/demo", "auth_provider_production", [fresh({ id: "a", value: "Keycloak 26" })]),
    key("project/demo", "database_engine", [fresh({ id: "b", value: "PostgreSQL 16" })]),
    key("project/demo", "hosting_region", [fresh({ id: "c", value: "Hetzner fsn1", unit: "DC" })]),
  ];
  const byAttribute = buildFactsView(keys, { now: NOW, query: "auth provider" });
  assert.equal(byAttribute.matched, 1);
  assert.equal(byAttribute.subjects[0].items[0].title, "Auth provider production");
  const byValue = buildFactsView(keys, { now: NOW, query: "postgres" });
  assert.equal(byValue.matched, 1);
  assert.equal(byValue.subjects[0].items[0].attribute, "database_engine");
  assert.equal(buildFactsView(keys, { now: NOW, query: "fsn1 dc" }).matched, 1, "a value's unit is searched too");
  assert.equal(buildFactsView(keys, { now: NOW, query: "nothing like this" }).subjects.length, 0);
  assert.equal(buildFactsView(keys, { now: NOW, query: "   " }).matched, 3, "a blank search is no search");
});

test("stale facts are left out of the view unless asked for, and the status says so", () => {
  const stale = fresh({ id: "old", value: "Postgres 9", observed_at: "2024-01-01T00:00:00Z", last_confirmed_at: "2024-01-01T00:00:00Z" });
  const keys = [key("system/db", "version", [stale]), key("system/db", "backup", [fresh({ id: "b" })])];
  const hidden = buildFactsView(keys, { now: NOW });
  assert.equal(hidden.matched, 1);
  assert.equal(factsStatusText({ matched: hidden.matched, staleHidden: 1 }), "1 fact · 1 stale fact hidden");
  const shown = buildFactsView(keys, { now: NOW, showStale: true });
  assert.equal(shown.matched, 2);
  const [version] = shown.subjects[0].items.filter((i) => i.attribute === "version");
  assert.equal(version.stale, true, "shown, and badged Stale");
  assert.equal(factsStatusText({ matched: 2, asOf: "2025-06-01", staleHidden: 0 }), `2 facts · as of ${formatDay("2025-06-01")}`);
});

test("a long value is clamped with Show more; a short one is not", () => {
  const long = "x".repeat(200);
  const view = buildFactsView(
    [key("project/demo", "notes", [fresh({ id: "n", value: long })]), key("project/demo", "short", [fresh({ id: "s", value: "ok" })])],
    { now: NOW },
  );
  const sections = factsSectionsDescription(view, { now: NOW });
  assert.equal(classText(sections, "fact__more").length, 1);
  assert.equal(classText(sections, "fact__value--clamp").length, 1);
});

test("a subject past the first fifty facts shows Show all until it is expanded", () => {
  const keys = Array.from({ length: LIST_LIMIT + 3 }, (_, i) =>
    key("project/big", `attr_${String(i).padStart(3, "0")}`, [fresh({ id: `f${i}` })]),
  );
  const view = buildFactsView(keys, { now: NOW });
  const closed = factsSectionsDescription(view, { now: NOW });
  assert.equal(classText(closed, "fact").length, LIST_LIMIT);
  assert.deepEqual(classText(closed, "facts-more"), [`Show all ${LIST_LIMIT + 3}`]);
  const open = factsSectionsDescription(view, { now: NOW, expanded: new Set(["project/big"]) });
  assert.equal(classText(open, "fact").length, LIST_LIMIT + 3);
  assert.deepEqual(classText(open, "facts-more"), []);
});

test("the view speaks plain English: no raw keys or jargon in the visible text", () => {
  const keys = [
    key("project/demo", "auth_provider_production", [fresh({ id: "a", valid_from: "2026-10-02" })], [fresh({ id: "a" })]),
  ];
  const visible = text(factsSectionsDescription(buildFactsView(keys, { now: NOW }), { now: NOW }));
  assert.doesNotMatch(visible, /valid_from|valid_to|observed_at|scope/u);
  // The raw key lives in the title tooltip and the Details, never as the title.
  assert.match(visible, /Auth provider production/u);
});
