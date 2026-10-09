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
  connectionViews,
  maskedKey,
  statusView,
  renderModelCards,
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

test("connected models render as cards in role order, never with a key", () => {
  const rows = [
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
  const views = connectionViews(rows);
  assert.deepEqual(views.map((v) => v.role), ["main", "research"]);
  assert.equal(views[0].roleLabel, "Main");
  assert.equal(views[0].providerLabel, "Anthropic");
  assert.equal(views[0].key, "••••a9Q2");
  assert.deepEqual(views[0].status, { state: "live", label: "Works", detail: "" });
  assert.equal(views[1].providerLabel, "Kimi", "a row finds its catalog name");
  assert.equal(views[1].status.state, "warn");
  assert.match(views[1].status.detail, /context size/);
  for (const view of views) assert.ok(!JSON.stringify(view).includes(FIXTURE_KEY));

  // The DOM half: elements and text only, and the two actions per card.
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
  // createElementNS is for the provider marks; the fake records them like any element.
  renderModelCards(container, views, { createElement: node, createElementNS: (_ns, tag) => node(tag) });
  assert.equal(container.children.length, 2);
  const buttons = made.filter((el) => el.tag === "button");
  assert.deepEqual(
    buttons.map((b) => `${b.attrs["data-action"]}:${b.attrs["data-role"]}`),
    ["replace:main", "disconnect:main", "replace:research", "disconnect:research"],
  );
  for (const el of made) assert.ok(!el.textContent.includes(FIXTURE_KEY));

  const empty = { children: [], replaceChildren() { this.children = []; }, appendChild(c) { this.children.push(c); } };
  renderModelCards(empty, [], { createElement: node });
  assert.equal(empty.children.length, 1, "an empty state, not an empty box");
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
  // The key field is a password field until the person asks to see it.
  assert.match(SETTINGS_HTML, /id="api-key"\s+type="password"/);
});
