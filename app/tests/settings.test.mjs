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

test("the six sections the issue asks for are present", () => {
  assert.deepEqual(SECTIONS, [
    "models",
    "proactivity",
    "tools",
    "automations",
    "skills",
    "colonizer",
  ]);
  for (const section of SECTIONS) assert.ok(isSection(section));
  assert.ok(!isSection("billing"));
});

test("a models change is a PUT to the role's path", () => {
  const request = settingRequest("models", "main", {
    provider: "deepseek",
    base_url: "https://api.deepseek.com/v1",
    api_key: "sk-test",
    model: "deepseek-chat",
    fallback_to_managed: false,
  });
  assert.equal(request.wired, true);
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
    api_key: "sk-test",
    model: "gpt-x",
    fallback_to_managed: true,
  });
  // The handler reads these flat.
  assert.equal(request.body.provider, "openai-compatible");
  assert.equal(request.body.base_url, null);
  assert.equal(request.body.api_key, "sk-test");
  assert.equal(request.body.model, "gpt-x");
  assert.equal(request.body.fallback_to_managed, true);
});

test("the body carries section, key and value next to the change", () => {
  // This is the audit triple: a server can attribute the write without
  // parsing the path.
  const value = { provider: "deepseek", api_key: "sk-test", model: "m" };
  const request = settingRequest("models", "triage", value);
  assert.equal(request.body.section, "models");
  assert.equal(request.body.key, "triage");
  assert.deepEqual(request.body.value, value);
});

test("a non-models change carries the same triple", () => {
  const request = settingRequest("proactivity", "nightly-evolve", true);
  assert.deepEqual(request.body, {
    section: "proactivity",
    key: "nightly-evolve",
    value: true,
  });
});

test("a section with no server route is not wired, and has no path", () => {
  for (const section of ["proactivity", "tools", "automations", "skills", "colonizer"]) {
    const request = settingRequest(section, "anything", true);
    assert.equal(request.wired, false, section);
    assert.equal(request.method, null);
    assert.equal(request.path, null);
    assert.deepEqual(request.body, {
      section,
      key: "anything",
      value: true,
    });
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

test("the outcome text says plainly when nothing was sent", () => {
  const unwired = settingRequest("tools", "mcp-tools", false);
  const wired = settingRequest("models", "main", {});
  assert.match(describeOutcome(unwired, false), /no server route yet/);
  assert.match(describeOutcome(unwired, false), /tools\.mcp-tools/);
  assert.match(describeOutcome(wired, true), /models\.main saved/);
  assert.match(describeOutcome(wired, false), /could not be saved/);
});