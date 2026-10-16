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
