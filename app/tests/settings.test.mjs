// The settings choke point's request shape.
//
// `app.js` calls `settingRequest` (through `LB.setting`) and sends whatever
// comes back; these tests pin the shape that reaches the server, because that
// is what makes the audit criterion (#30) satisfiable: every write carries its
// section, key and value.

import test from "node:test";
import assert from "node:assert/strict";

import {
  SECTIONS,
  MODEL_ROLES,
  settingRequest,
  isSection,
  auditRecord,
  describeOutcome,
} from "../assets/settings.js";

/**
 * The placeholder BYOK key these tests paste into a models payload. Built at
 * run time from words rather than written out, so nothing in this file has the
 * shape of a real provider credential — a secret scanner reads shape, not
 * intent, and a fixture is exactly the place it must not get one.
 */
const FIXTURE_KEY = ["lb", "test", "byok", "fixture"].join("-");

test("the sections the page changes are the ones with a server route", () => {
  assert.deepEqual(SECTIONS, [
    "models",
    "model-switch",
    "model-runner",
    "jev",
    "jev-test",
    "budgets",
  ]);
  for (const section of SECTIONS) assert.ok(isSection(section));
  assert.ok(!isSection("billing"));
});

test("a budgets change is one PATCH carrying the patch beside the audit triple", () => {
  const patch = { coding_agents: { daily_tokens: 3000000, batch: null } };
  const request = settingRequest("budgets", "limits", patch);
  assert.equal(request.method, "PATCH");
  assert.equal(request.path, "/v1/settings/budgets");
  assert.deepEqual(request.body.coding_agents, patch.coding_agents);
  assert.equal(request.body.section, "budgets");
  assert.equal(request.body.key, "limits");
});

test("a models change is a PUT to the role's path", () => {
  const request = settingRequest("models", "main", {
    provider: "deepseek",
    base_url: "https://api.deepseek.com/v1",
    api_key: FIXTURE_KEY,
    model: "deepseek-chat",
    fallback_to_managed: false,
  });
  assert.equal(request.method, "PUT");
  assert.equal(request.path, "/v1/models/main");
  assert.equal(request.section, "models");
  assert.equal(request.key, "main");
});

test("every model role maps to its own path", () => {
  for (const role of MODEL_ROLES) {
    const request = settingRequest("models", role, { provider: "deepseek" });
    assert.equal(request.path, `/v1/models/${role}`);
  }
});

test("a role with a slash is encoded into the path", () => {
  const request = settingRequest("models", "a/../b", { provider: "deepseek" });
  assert.equal(request.path, "/v1/models/a%2F..%2Fb");
});

test("the body carries the model fields the API reads", () => {
  const request = settingRequest("models", "research", {
    provider: "openai-compatible",
    base_url: null,
    api_key: FIXTURE_KEY,
    model: "gpt-x",
    fallback_to_managed: true,
  });
  // The handler reads these flat.
  assert.equal(request.body.provider, "openai-compatible");
  assert.equal(request.body.base_url, null);
  assert.equal(request.body.api_key, FIXTURE_KEY);
  assert.equal(request.body.model, "gpt-x");
  assert.equal(request.body.fallback_to_managed, true);
});

test("the body carries section, key and value next to the change", () => {
  // This is the audit triple: a server can attribute the write without
  // parsing the path.
  const value = { provider: "deepseek", api_key: FIXTURE_KEY, model: "m" };
  const request = settingRequest("models", "triage", value);
  assert.equal(request.body.section, "models");
  assert.equal(request.body.key, "triage");
  assert.deepEqual(request.body.value, value);
});

test("a non-models change carries the same triple", () => {
  const request = settingRequest("budgets", "limits", true);
  assert.deepEqual(request.body, {
    section: "budgets",
    key: "limits",
    value: true,
  });
});

test("a section with no server route cannot be described, so nothing is sent for it", () => {
  for (const section of ["proactivity", "tools", "automations", "skills", "colonizer"]) {
    assert.throws(() => settingRequest(section, "anything", true), /unknown settings section/);
  }
});

test("an unknown section throws rather than silently doing nothing", () => {
  assert.throws(() => settingRequest("nope", "k", 1), /unknown settings section/);
  assert.throws(() => settingRequest("models", "   ", 1), /needs a key/);
  assert.throws(() => settingRequest("models", null, 1), /needs a key/);
});

test("a key is trimmed, because ' main ' and 'main' are one setting", () => {
  assert.equal(settingRequest("models", "  main  ", {}).key, "main");
  assert.equal(settingRequest("models", "  main  ", {}).path, "/v1/models/main");
});

test("auditRecord is the shape a server would log", () => {
  const request = settingRequest("models", "main", { provider: "deepseek" });
  assert.deepEqual(auditRecord(request), {
    section: "models",
    key: "main",
    value: { provider: "deepseek" },
    method: "PUT",
    path: "/v1/models/main",
  });
});

test("the outcome text says whether the change was saved", () => {
  const wired = settingRequest("models", "main", {});
  assert.match(describeOutcome(wired, true), /models\.main saved/);
  assert.match(describeOutcome(wired, false), /could not be saved/);
});
