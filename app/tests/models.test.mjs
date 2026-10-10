// The Models section and the settings page's copy.
//
// The provider and role lists are checked against the Rust handler that
// accepts them, so the page cannot offer a choice the server refuses (it once
// sent `openai-compatible`, which the server has never accepted).

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

import catalog from "../assets/providers.json" with { type: "json" };
import {
  ROLES,
  PROVIDERS,
  POPULAR,
  providerInfo,
  providerHost,
  providerForConnection,
  searchProviders,
  fillTemplate,
  keyHint,
  discoverRequest,
  validateConnect,
  slotView,
  slotViews,
  maskedKey,
  statusView,
  renderModelSlots,
} from "../assets/models.js";
import { settingRequest, describeOutcome } from "../assets/settings.js";
import { TOKEN_SCOPES, tokenCreateRequest } from "../assets/tokens.js";

const HANDLERS = readFileSync(
  new URL("../../crates/livingbrain-models/src/handlers.rs", import.meta.url),
  "utf8",
);
const CATALOG = catalog;
const SETTINGS_HTML = readFileSync(new URL("../settings.html", import.meta.url), "utf8");

/** A placeholder key built from words, as in the other test files. */
const FIXTURE_KEY = ["lb", "test", "byok", "fixture"].join("-");

const good = {
  role: "main",
  provider: "openai",
  baseUrl: "",
  apiKey: FIXTURE_KEY,
  model: "gpt-4.1",
};

test("the roles are exactly the ones the server accepts", () => {
  const rust = HANDLERS.match(/const ROLES: &\[&str\] = &\[([^\]]+)\]/)[1]
    .match(/"([^"]+)"/g)
    .map((s) => s.slice(1, -1));
  assert.deepEqual([...ROLES.map((r) => r.id)].sort(), [...rust].sort());
  for (const role of ROLES) {
    assert.ok(role.label && role.detail, `${role.id} has a label and a line`);
    assert.ok(role.hint, `${role.id} has a one-glance hint for its slot card`);
    assert.notEqual(role.label, role.id, "a human label, not the raw id");
  }
});

test("the page and the server read the same catalog", () => {
  const rust = readFileSync(
    new URL("../../crates/livingbrain-models/src/catalog.rs", import.meta.url),
    "utf8",
  );
  assert.match(rust, /include_str!\("\.\.\/\.\.\/\.\.\/app\/assets\/providers\.json"\)/);
  assert.equal(PROVIDERS.length, CATALOG.builtin.length + CATALOG.providers.length);
  // Every popular card is a real entry, and Anthropic and OpenAI are there.
  for (const id of POPULAR) assert.ok(providerInfo(id), id);
  assert.deepEqual(POPULAR.slice(0, 2), ["anthropic", "openai"]);
  // The page hard-codes no model names: suggestions come from the catalog or
  // from the provider, never from a list in the app that goes stale.
  const source = readFileSync(new URL("../assets/models.js", import.meta.url), "utf8");
  assert.doesNotMatch(source, /gpt-|claude-|gemini-|llama-|deepseek-chat/);
});

test("a catalog provider sends its id and lets the server fill the endpoint", () => {
  const preset = validateConnect(good);
  assert.equal(preset.ok, true, JSON.stringify(preset.errors));
  assert.equal(preset.role, "main");
  assert.deepEqual(preset.value, {
    provider: "openai",
    api_key: FIXTURE_KEY,
    model: "gpt-4.1",
    fallback_to_managed: false,
  });
  // An unlocked, edited endpoint travels as base_url.
  const edited = validateConnect({
    ...good,
    provider: "minimax-en",
    editUrl: true,
    baseUrl: "https://api.minimax.io/anthropic/",
  });
  assert.equal(edited.value.base_url, "https://api.minimax.io/anthropic");
  // A URL variable is asked for, checked, and sent for the server to fill.
  const kat = { ...good, provider: "kat-coder" };
  assert.match(validateConnect(kat).errors["var:ENDPOINT_ID"], /Vanchin endpoint ID/);
  assert.match(
    validateConnect({ ...kat, variables: { ENDPOINT_ID: "../x" } }).errors["var:ENDPOINT_ID"],
    /only/,
  );
  const filled = validateConnect({ ...kat, variables: { ENDPOINT_ID: "ep-1" } });
  assert.deepEqual(filled.value.variables, { ENDPOINT_ID: "ep-1" });
  assert.equal(filled.value.base_url, undefined);
  assert.equal(
    fillTemplate(providerInfo("kat-coder").base_url, { ENDPOINT_ID: "ep-1" }),
    "https://vanchin.streamlake.ai/api/gateway/v1/endpoints/ep-1/claude-code-proxy",
  );
});

test("a custom endpoint names its wire and auth", () => {
  const base = { ...good, provider: "custom", baseUrl: "https://gw.example.com" };
  const missing = validateConnect(base).errors;
  assert.ok(missing.wire && missing.auth, JSON.stringify(missing));
  const ok = validateConnect({ ...base, wire: "anthropic", auth: "x-api-key" });
  assert.deepEqual(ok.value, {
    provider: "custom",
    api_key: FIXTURE_KEY,
    model: "gpt-4.1",
    fallback_to_managed: false,
    base_url: "https://gw.example.com",
    wire: "anthropic",
    auth: "x-api-key",
  });
  assert.deepEqual(discoverRequest(ok.value), {
    method: "POST",
    path: "/v1/models/discover",
    body: {
      provider: "custom",
      api_key: FIXTURE_KEY,
      base_url: "https://gw.example.com",
      wire: "anthropic",
      auth: "x-api-key",
    },
  });
});

test("the provider search finds by name, id or host, sorted by name", () => {
  assert.ok(searchProviders("").length === PROVIDERS.length);
  assert.deepEqual(
    searchProviders("api.x.ai").map((p) => p.id),
    ["xai-grok"],
  );
  const tencent = searchProviders("tencent intl").map((p) => p.name);
  assert.ok(tencent.length >= 2 && tencent.every((n) => /Tencent/.test(n) && /Intl/.test(n)));
  const names = searchProviders("").slice(2).map((p) => p.name);
  assert.deepEqual(names, [...names].sort((a, b) => a.localeCompare(b, "en", { sensitivity: "base" })));
  assert.equal(providerHost(providerInfo("kat-coder")), "vanchin.streamlake.ai");
  assert.equal(keyHint(providerInfo("anthropic")), "Sent in an x-api-key header.");
  assert.equal(keyHint(providerInfo("openai")), "Sent as a Bearer token.");
});

test("each field's mistake is named under that field", () => {
  const empty = validateConnect({ role: "", provider: "", apiKey: " ", model: "" });
  assert.equal(empty.ok, false);
  assert.equal(empty.value, null);
  assert.deepEqual(Object.keys(empty.errors).sort(), ["apiKey", "model", "provider", "role"]);

  const custom = (baseUrl) =>
    validateConnect({ ...good, provider: "custom", baseUrl, wire: "openai", auth: "bearer" }).errors;
  assert.match(custom("").baseUrl, /base URL/);
  assert.match(custom("https://h.example/${X}").baseUrl, /placeholder/);
  assert.match(custom("not a url").baseUrl, /not a URL/);
  assert.match(custom("http://api.example.com/v1").baseUrl, /https/);
  assert.match(custom("https://user:pw@api.example.com/v1").baseUrl, /API key field/);
  assert.equal(custom("https://api.example.com/v1").baseUrl, undefined);
  // A preset never asks for a URL, whatever the hidden field holds.
  assert.equal(validateConnect({ ...good, baseUrl: "junk" }).ok, true);
});

const SLOT_ROWS = [
  {
    role: "research",
    provider: "kimi",
    base_url: "https://api.moonshot.cn/anthropic",
    model: "kimi-k2",
    key: "…wxyz",
    status: "answers only",
    missing: ["context size"],
    api_key: FIXTURE_KEY,
  },
  { role: "main", provider: "anthropic", model: "claude-sonnet-4-5", key: "…a9Q2", status: "works" },
];

test("the three slots, in role order, name provider, model and masked key when set", () => {
  const views = slotViews(SLOT_ROWS);
  assert.deepEqual(views.map((v) => v.role), ["main", "triage", "research"], "three slots, Main first");
  const main = views[0];
  assert.equal(main.roleLabel, "Main");
  assert.equal(main.set, true);
  assert.equal(main.providerLabel, "Anthropic");
  assert.equal(main.key, "••••a9Q2");
  assert.equal(main.line, "Anthropic · claude-sonnet-4-5 · key ••••a9Q2");
  assert.deepEqual(main.status, { state: "live", label: "Works", detail: "" });

  const research = views[2];
  assert.equal(research.providerLabel, "Kimi", "a row finds its catalog name");
  assert.equal(research.status.state, "warn");
  assert.match(research.status.detail, /context size/);
  for (const view of views) assert.ok(!JSON.stringify(view).includes(FIXTURE_KEY));
});

test("an unset slot says what answers instead: Main, or the managed model", () => {
  // Triage is unset while Main is connected, so triage borrows Main.
  const triage = slotView(SLOT_ROWS, "triage");
  assert.equal(triage.set, false);
  assert.equal(triage.line, "Not set · uses Main");
  assert.equal(triage.status, null, "nothing to probe, nothing to badge");

  // Main's own unset state, and triage's when not even Main is set: the
  // managed model answers.
  assert.equal(slotView([], "main").line, "Not set · the managed model answers");
  assert.equal(slotView([], "triage").line, "Not set · the managed model answers");
  assert.equal(slotViews([]).every((v) => v.line === "Not set · the managed model answers"), true);

  // Only Research is set: Main is still unset, so Triage's fallback chain
  // (triage → main → managed) ends at the managed model too.
  assert.equal(
    slotView([{ role: "research", provider: "kimi", model: "kimi-k2" }], "triage").line,
    "Not set · the managed model answers",
  );
});

test("slots render as cards: Connect when unset, Change and Remove when set", () => {
  const views = slotViews(SLOT_ROWS);
  const made = [];
  const node = (tag) => {
    const el = {
      tag,
      className: "",
      textContent: "",
      attrs: {},
      children: [],
      setAttribute(name, value) {
        this.attrs[name] = value;
      },
      appendChild(child) {
        this.children.push(child);
      },
    };
    made.push(el);
    return el;
  };
  const container = { children: [], replaceChildren() { this.children = []; }, appendChild(c) { this.children.push(c); } };
  renderModelSlots(container, views, { createElement: node, createElementNS: (_ns, tag) => node(tag) });
  assert.equal(container.children.length, 3, "one card per role, always");
  const buttons = made.filter((el) => el.tag === "button");
  assert.deepEqual(
    buttons.map((b) => `${b.attrs["data-action"]}:${b.attrs["data-role"]}`),
    [
      "change:main",
      "remove:main",
      "connect:triage",
      "change:research",
      "remove:research",
    ],
  );
  for (const el of made) assert.ok(!el.textContent.includes(FIXTURE_KEY));

  // A plain "works" wears no pill; a probe that found something to say does.
  const pills = made
    .filter((el) => el.tag === "span" && el.className === "pill")
    .map((p) => p.attrs["data-state"]);
  assert.deepEqual(pills, ["warn"]);
});

test("small pieces: masked keys, statuses, and matching a row to its card", () => {
  assert.equal(maskedKey("…abcd"), "••••abcd");
  assert.equal(maskedKey(""), "••••");
  assert.equal(statusView({}).label, "Not checked");
  assert.equal(providerForConnection({ provider: "deepseek" }).id, "deepseek");
  assert.equal(providerForConnection({ provider: "custom", base_url: "https://x.example/v1" }).id, "custom");
  assert.equal(providerForConnection({ provider: "gone-provider" }).id, "custom");
});

test("disconnecting goes through LB.setting as a DELETE", () => {
  const request = settingRequest("models", "triage", null);
  assert.equal(request.method, "DELETE");
  assert.equal(request.path, "/v1/models/triage");
  assert.deepEqual(request.body, { section: "models", key: "triage", value: null });
  assert.equal(describeOutcome(request, true), "models.triage removed.");
  assert.equal(settingRequest("models", "triage", { model: "x" }).method, "PUT");
});

test("the token access choice is full read access or shared only", () => {
  assert.deepEqual(TOKEN_SCOPES.map((s) => s.value), ["", "shared"]);
  assert.deepEqual(tokenCreateRequest("ci", TOKEN_SCOPES[0].value).body, { name: "ci" });
  assert.deepEqual(tokenCreateRequest("ci", TOKEN_SCOPES[1].value).body, {
    name: "ci",
    scopes: ["shared"],
  });
});

test("the settings page speaks to people, not to developers", () => {
  for (const jargon of [
    "GET /v1",
    "PUT /v1",
    "DELETE /v1",
    "LB.setting",
    "mcp.livingbrain.wiki",
    "openai-compatible",
    "No server route yet",
  ]) {
    assert.ok(!SETTINGS_HTML.includes(jargon), `settings.html still says ${jargon}`);
  }
  // Nothing in "Coming soon" looks like a control.
  const soon = SETTINGS_HTML.slice(
    SETTINGS_HTML.indexOf('id="soon"'),
    SETTINGS_HTML.indexOf('id="stack"'),
  );
  assert.doesNotMatch(soon, /<(input|select|button|textarea)\b/);
  // "Built with" is collapsed.
  assert.match(SETTINGS_HTML, /<details class="disclosure" id="stack">/);
  // The ids the page script and the token rules rely on are still there.
  for (const id of [
    "settings-notice",
    "models-status",
    "model-rows",
    "api-key",
    "model-name",
    "base-url",
    "connect-model",
    "tokens-status",
    "token-rows",
    "token-name",
    "create-token",
    "token-reveal",
    "token-value",
    "copy-token",
    "dismiss-token",
    "mcp-url",
    "stack-rows",
    "stack-venture",
    "stack-subprocessors",
    "stack-source",
  ]) {
    assert.match(SETTINGS_HTML, new RegExp(`id="${id}"`), id);
  }
  // The slots are the only role chooser; the form's "What is it for?" radio
  // is gone, and the heading names the slot the form is for.
  assert.match(SETTINGS_HTML, /id="connect-title"/);
  assert.doesNotMatch(SETTINGS_HTML, /id="role-choices"/);
  assert.doesNotMatch(SETTINGS_HTML, /id="err-role"/);
  // The key field is a password field until the person asks to see it.
  assert.match(SETTINGS_HTML, /id="api-key"\s+type="password"/);
});
