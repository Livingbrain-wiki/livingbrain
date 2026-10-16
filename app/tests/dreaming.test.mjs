// The Dreaming page (`dreaming.html`): the sidebar item, the words the log
// speaks in, the status line, what a save sends, the model choices, and the
// page itself: a read-only view for a member, and the switch and model a
// save sends for an owner.

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

import {
  ago,
  describeDecision,
  dreamingPage,
  humanKey,
  logPath,
  modelChoices,
  settingsPatch,
  statusLine,
} from "../assets/dreaming.js";
import { ICONS, NAV, activeId, navItems } from "../assets/nav.js";

const NOW = Date.parse("2027-01-15T08:00:00Z");
const text = (parts) => parts.map((part) => part.text).join("");
const bold = (parts) => parts.filter((part) => part.strong).map((part) => part.text);
const JARGON = /\b(supersede[ds]?|scope|cursor|op|retract|rename|decision)\b/i;

test("Dreaming is in the Brain group, after Facts, with an icon and a page", () => {
  const brain = NAV.find((group) => group.id === "brain");
  const ids = brain.items.map((item) => item.id);
  assert.deepEqual(ids.slice(ids.indexOf("facts"), ids.indexOf("facts") + 2), ["facts", "dreaming"]);
  const item = navItems().find((entry) => entry.id === "dreaming");
  assert.equal(item.href, "dreaming.html");
  assert.equal(item.label, "Dreaming");
  assert.equal(item.hint, "How the brain tidies its facts");
  assert.ok(ICONS[item.icon], "the item has an icon");
  assert.equal(activeId({ pathname: "/dreaming.html" }), "dreaming");
  assert.equal(activeId({ pathname: "/dreaming" }), "dreaming", "the clean URL too");
});

test("a change reads as a sentence, with the names in bold", () => {
  const replaced = describeDecision(
    {
      op: "supersede",
      applied: true,
      subject: "project/earthos-world",
      at: "2027-01-15T06:00:00Z",
      fact: { attribute: "auth_provider_decision", value: "Clerk" },
      by: { attribute: "auth_provider_production", value: "Clerk prod" },
    },
    NOW,
  );
  assert.equal(replaced.muted, false);
  assert.equal(text(replaced.parts), "Replaced Auth provider decision with Auth provider production");
  assert.deepEqual(bold(replaced.parts), ["Auth provider decision", "Auth provider production"]);
  assert.equal(replaced.detail, "was Clerk, now Clerk prod");
  assert.equal(replaced.when, "2 h ago");
  assert.equal(replaced.subject, "project/earthos-world");

  const removed = describeDecision(
    { op: "retract", applied: true, reason: "not-state", fact: { attribute: "issue_96_status" }, at: "2027-01-15T07:59:00Z" },
    NOW,
  );
  assert.equal(text(removed.parts), "Removed Issue 96 status — a one-off note");
  assert.equal(removed.when, "1 min ago");

  const duplicate = describeDecision(
    { op: "retract", applied: true, reason: "duplicate", fact: { attribute: "db" } },
    NOW,
  );
  assert.equal(text(duplicate.parts), "Removed Db — a duplicate");

  const renamed = describeDecision(
    { op: "rename", applied: true, fact: { attribute: "x" }, new_attribute: "deploy_target" },
    NOW,
  );
  assert.equal(text(renamed.parts), "Renamed X → Deploy target");

  const ended = describeDecision(
    { op: "end", applied: true, fact: { attribute: "shoe_size" }, new_valid_to: "2026-10" },
    NOW,
  );
  assert.equal(text(ended.parts), "Ended Shoe size from 2026-10");
});

test("a refused change is muted and says why in plain words", () => {
  const refused = describeDecision(
    { op: "end", applied: false, reason: "not-older", fact: { attribute: "auth_provider" } },
    NOW,
  );
  assert.equal(refused.muted, true);
  assert.equal(text(refused.parts), "Skipped Auth provider: the older fact must start first");
  const unknown = describeDecision({ op: "retract", applied: false, reason: "weird", fact: { attribute: "a" } }, NOW);
  assert.equal(text(unknown.parts), "Skipped A: it was not applied");
});

test("no page copy names the internals", () => {
  const rows = [
    { op: "supersede", applied: true, fact: { attribute: "a" }, by: { attribute: "b" } },
    { op: "end", applied: true, fact: { attribute: "a" } },
    { op: "retract", applied: true, reason: "not-state", fact: { attribute: "a" } },
    { op: "rename", applied: true, fact: { attribute: "a" }, new_attribute: "b" },
    { op: "end", applied: false, reason: "not-older", fact: { attribute: "a" } },
  ];
  for (const row of rows) {
    const line = describeDecision(row, NOW);
    assert.doesNotMatch(text(line.parts), JARGON, text(line.parts));
  }
  assert.equal(humanKey("auth_provider_decision"), "Auth provider decision");
  assert.equal(humanKey(""), "a fact");
});

test("ago reads a time as the page says it", () => {
  assert.equal(ago("2027-01-15T07:59:45Z", NOW), "just now");
  assert.equal(ago("2027-01-15T07:48:00Z", NOW), "12 min ago");
  assert.equal(ago("2027-01-15T06:00:00Z", NOW), "2 h ago");
  assert.equal(ago("2027-01-14T08:00:00Z", NOW), "1 day ago");
  assert.equal(ago("2026-12-01T08:00:00Z", NOW), "2026-12-01");
  assert.equal(ago("not a time", NOW), "");
});

test("the status strip says when it ran, what it reviewed and what changed", () => {
  const line = statusLine(
    {
      last_run_at: "2027-01-15T07:48:00Z",
      reviewed_week: 2,
      queued: 0,
      changes_week: { retract: 30, supersede: 7 },
      run_requested: false,
    },
    NOW,
  );
  assert.equal(line, "Last ran 12 min ago · 2 subjects reviewed this week · 37 changes this week");
  assert.equal(
    statusLine({ reviewed_week: 1, changes_week: {}, queued: 3, run_requested: true }, NOW),
    "Has not run yet · 1 subject reviewed this week · 0 changes this week · 3 waiting · a run is asked for the next tick",
  );
});

test("a save sends only what changed", () => {
  const current = { enabled: true, rules_enabled: true, model: { role: "main" } };
  assert.deepEqual(settingsPatch(current, { enabled: false, rules_enabled: true, model: "main" }), { enabled: false });
  assert.deepEqual(settingsPatch(current, { enabled: true, rules_enabled: false, model: "triage" }), {
    rules_enabled: false,
    model: "triage",
  });
  assert.deepEqual(settingsPatch(current, { enabled: true, rules_enabled: true, model: "main" }), {});
});

test("the model picker offers only the slots the workspace connected", () => {
  const choices = modelChoices({
    models: [
      { role: "main", usable: true, provider: "acme", model: "big" },
      { role: "triage", usable: false },
      { role: "research", usable: false },
    ],
  });
  assert.deepEqual(choices, [
    { value: "main", label: "Main model: big", disabled: false },
    { value: "triage", label: "Triage model: not connected", disabled: true },
    { value: "research", label: "Research model: not connected", disabled: true },
  ]);
});

test("the log pages by a cursor the server gives", () => {
  assert.equal(logPath(null), "/v1/dreaming/decisions?limit=20");
  assert.equal(logPath("2027-01-14T10:00:00Z|user-a|1"), "/v1/dreaming/decisions?limit=20&before=2027-01-14T10%3A00%3A00Z%7Cuser-a%7C1");
});

/* --- the page, over a recording document ---------------------------------- */

function fakeDocument(ids) {
  const make = (tag) => ({
    tag,
    children: [],
    attrs: {},
    dataset: {},
    listeners: {},
    textContent: "",
    className: "",
    value: "",
    checked: false,
    disabled: false,
    hidden: false,
    setAttribute(name, value) {
      this.attrs[name] = String(value);
    },
    appendChild(child) {
      this.children.push(child);
      return child;
    },
    replaceChildren(...kids) {
      this.children = kids;
    },
    addEventListener(type, fn) {
      (this.listeners[type] ||= []).push(fn);
    },
    async fire(type, extra = {}) {
      const event = { type, preventDefault() {}, ...extra };
      for (const fn of this.listeners[type] || []) await fn(event);
    },
  });
  const byId = Object.fromEntries(ids.map((id) => [id, make("div")]));
  for (const id of ids) byId[id].id = id;
  byId["dream-model"].tag = "select";
  byId["dream-model"].options = [];
  return {
    byId,
    getElementById: (id) => byId[id] || null,
    createElement: (tag) => make(tag),
    createTextNode: (value) => ({ textNode: String(value) }),
  };
}

const PAGE_IDS = [
  "dream-form",
  "dream-enabled",
  "dream-enabled-hint",
  "dream-model",
  "dream-rules",
  "dream-rules-hint",
  "dream-save",
  "dream-run",
  "dream-readonly",
  "dream-notice",
  "dream-strip",
  "dream-log",
  "dream-log-empty",
  "dream-more",
];

function view(overrides = {}) {
  return {
    enabled: true,
    rules_enabled: true,
    model: { role: "main", usable: true, provider: "acme", model: "big" },
    models: [
      { role: "main", usable: true, provider: "acme", model: "big" },
      { role: "triage", usable: false },
      { role: "research", usable: false },
    ],
    editable: true,
    status: { queued: 0, reviewed_week: 2, last_run_at: "2027-01-15T07:48:00Z", changes_week: { retract: 1 } },
    ...overrides,
  };
}

/** `api` that answers from a script and records every call. */
function scripted(answers) {
  const calls = [];
  const api = async (path, options = {}) => {
    calls.push({ path, method: options.method || "GET", body: options.body });
    const answer = answers[`${options.method || "GET"} ${path.split("?")[0]}`];
    if (typeof answer === "function") return answer(options);
    return answer;
  };
  return { api, calls };
}

test("a member sees the settings read-only, and the log", async () => {
  const doc = fakeDocument(PAGE_IDS);
  const { api, calls } = scripted({
    "GET /v1/dreaming": view({ editable: false }),
    "GET /v1/dreaming/decisions": {
      decisions: [
        { op: "retract", applied: true, reason: "not-state", subject: "project/demo", at: "2027-01-15T07:00:00Z", fact: { attribute: "issue_96_status", value: "open" }, where: "personal" },
      ],
      next: "2027-01-15T07:00:00Z|user-a|0",
    },
  });
  await dreamingPage(api, doc, () => NOW);
  assert.equal(doc.byId["dream-enabled"].checked, true);
  assert.equal(doc.byId["dream-enabled"].disabled, true, "a member cannot switch it");
  assert.equal(doc.byId["dream-model"].disabled, true);
  assert.equal(doc.byId["dream-save"].hidden, true, "a member has no save");
  assert.equal(doc.byId["dream-run"].hidden, true, "a member has no run");
  assert.equal(doc.byId["dream-readonly"].hidden, false, "says why");
  assert.equal(doc.byId["dream-strip"].textContent, "Last ran 12 min ago · 2 subjects reviewed this week · 1 change this week");
  assert.equal(doc.byId["dream-log"].children.length, 1);
  assert.equal(doc.byId["dream-more"].hidden, false, "there is a next page");
  assert.deepEqual(calls.map((call) => call.path), ["/v1/dreaming", "/v1/dreaming/decisions?limit=20"]);
});

test("an owner switches dreaming off and sends only that", async () => {
  const doc = fakeDocument(PAGE_IDS);
  let saved = null;
  const { api, calls } = scripted({
    "GET /v1/dreaming": view(),
    "GET /v1/dreaming/decisions": { decisions: [], next: null },
    "PUT /v1/dreaming": (options) => {
      saved = options.body;
      return view({ enabled: false });
    },
  });
  await dreamingPage(api, doc, () => NOW);
  assert.equal(doc.byId["dream-enabled"].disabled, false);
  assert.equal(doc.byId["dream-log-empty"].hidden, false, "an empty log says so");
  doc.byId["dream-enabled"].checked = false;
  await doc.byId["dream-form"].fire("submit");
  assert.deepEqual(saved, { enabled: false });
  assert.equal(doc.byId["dream-enabled"].checked, false);
  assert.equal(doc.byId["dream-notice"].textContent, "Saved.");
  assert.equal(calls.filter((call) => call.method === "PUT").length, 1);
});

test("choosing a connected model is saved; an unconnected one cannot be picked", async () => {
  const doc = fakeDocument(PAGE_IDS);
  const { api } = scripted({
    "GET /v1/dreaming": view({
      models: [
        { role: "main", usable: true, provider: "acme", model: "big" },
        { role: "triage", usable: true, provider: "acme", model: "small" },
        { role: "research", usable: false },
      ],
    }),
    "GET /v1/dreaming/decisions": { decisions: [], next: null },
  });
  await dreamingPage(api, doc, () => NOW);
  const options = doc.byId["dream-model"].children;
  assert.equal(options.length, 3);
  assert.equal(options[2].attrs.disabled, "", "research is not connected");
  assert.equal(options[0].attrs.disabled, undefined);
  assert.equal(doc.byId["dream-model"].value, "main");
});

test("a member's save is refused and shown, not hidden", async () => {
  const doc = fakeDocument(PAGE_IDS);
  const { api } = scripted({
    "GET /v1/dreaming": view(),
    "GET /v1/dreaming/decisions": { decisions: [], next: null },
    "PUT /v1/dreaming": () => {
      throw new Error("Only workspace owners and admins can change dreaming.");
    },
  });
  await dreamingPage(api, doc, () => NOW);
  doc.byId["dream-rules"].checked = false;
  await doc.byId["dream-form"].fire("submit");
  assert.equal(doc.byId["dream-notice"].dataset.tone, "bad");
  assert.match(doc.byId["dream-notice"].textContent, /owners and admins/);
});
