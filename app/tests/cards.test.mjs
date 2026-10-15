// Settings → Models as provider cards: the card file, the grid, the model
// pickers, and the device runner's settings writes.
//
// `model-cards.json` is read by the page and compiled into the server
// (`crates/livingbrain-models/src/curated.rs`), so the checks here are the
// page's half of one contract.

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync, existsSync } from "node:fs";

import cardFile from "../assets/model-cards.json" with { type: "json" };
import {
  CARDS,
  BADGES,
  providerInfo,
  cardInfo,
  cardForConnection,
  runnerCardFor,
  protocolLabel,
  formatContext,
  modelOptionLabel,
  modelOptions,
  cardViews,
  renderModelCards,
  mainModelChoices,
  lastSeenText,
  runnerValue,
  runnerCommand,
  slotView,
  JEV_CARD,
  JEV_PRIVACY,
  jevView,
  jevChange,
  jevTestBody,
  jevTestMessage,
} from "../assets/models.js";
import { settingRequest } from "../assets/settings.js";

const CSS = readFileSync(new URL("../assets/app.css", import.meta.url), "utf8");
const APP = readFileSync(new URL("../assets/app.js", import.meta.url), "utf8");
const HTML = readFileSync(new URL("../settings.html", import.meta.url), "utf8");
const CURATED_RS = readFileSync(
  new URL("../../crates/livingbrain-models/src/curated.rs", import.meta.url),
  "utf8",
);

const ROWS = [
  { role: "main", provider: "anthropic", model: "claude-opus-5-5", key: "…a1b2", status: "works", missing: [] },
  { role: "research", provider: "xai-grok", model: "grok-x", key: "…9f8e", status: "works", missing: [] },
];
const MINE = [{ role: "triage", kind: "local_runner", agent: "codex", model: "gpt-5.5", settings: "isolated" }];

test("the card file is the one the server compiles in, and every card is complete", () => {
  assert.match(CURATED_RS, /include_str!\("\.\.\/\.\.\/\.\.\/app\/assets\/model-cards\.json"\)/);
  assert.deepEqual(
    CARDS.map((c) => c.id),
    [
      "anthropic",
      "claude-subscription",
      "openai",
      "chatgpt-subscription",
      "zai",
      "deepseek",
      "openrouter",
      "minimax",
      "kimi",
      "qwen",
      "custom",
      "jev",
    ],
  );
  for (const card of cardFile.cards) {
    assert.ok(card.name && card.description, card.id);
    assert.ok(card.description.length <= 70, `${card.id}: one line, not a paragraph`);
    assert.ok(card.badges.length && card.badges.every((b) => BADGES.includes(b)), card.id);
    assert.ok(card.logo_source, `${card.id} records where its logo came from`);
    const file = new URL(`../${card.logo}`, import.meta.url);
    assert.ok(existsSync(file), `${card.logo} is self-hosted`);
    const svg = readFileSync(file, "utf8");
    assert.doesNotMatch(svg, /<script|on\w+=|href=/i, `${card.logo} is plain artwork`);
    const name = card.logo.replace(/^.*\//, "").replace(/\.svg$/, "");
    assert.ok(CSS.includes(`[data-logo="${name}"]`), `app.css masks ${name}`);
    if (card.kind === "api") {
      assert.ok(providerInfo(card.provider), `${card.provider} is a catalog provider`);
      assert.ok(["anthropic", "openai"].includes(card.protocol), card.id);
      assert.ok(card.models.length, `${card.id} has curated models`);
    }
    if (card.kind === "classifier") {
      assert.equal(card.provider, undefined, "Jev is no job slot's provider");
      assert.deepEqual(card.badges, ["Optional"]);
      assert.match(card.description, /^Optional · triage classifier/);
      assert.match(card.detail, /saves tokens by skipping\/condensing trivial sessions before the main model compiles them/);
    }
    if (card.kind === "runner") {
      assert.ok(["claude", "codex"].includes(card.runner.agent));
      assert.equal(card.provider, undefined, "a subscription has no server-side provider");
      assert.deepEqual(card.badges, ["Subscription (runs on your Mac)"]);
    }
    for (const model of card.models) {
      assert.ok(model.id && typeof model.id === "string");
      if (model.context !== undefined) assert.ok(Number.isInteger(model.context));
    }
  }
  // Z.AI's Coding Plan is a runner route with the member's own settings, and says why.
  const zai = cardInfo("zai");
  assert.equal(zai.coding_plan.runner.settings, "user");
  assert.match(zai.coding_plan.note, /officially supported coding tools/);
  assert.match(zai.coding_plan.source, /^https:\/\/docs\.z\.ai\//);
});

test("the wire is said in the sheet only, never as a card's subtitle", () => {
  assert.equal(protocolLabel(cardInfo("zai")), "Protocol: Anthropic-compatible");
  assert.equal(protocolLabel(cardInfo("openai")), "Protocol: OpenAI-compatible");
  assert.equal(protocolLabel(cardInfo("claude-subscription")), "");
  assert.equal(protocolLabel(cardInfo("custom"), "openai"), "Protocol: OpenAI-compatible");
  // The old bug: every card read "Anthropic API".
  assert.doesNotMatch(APP, /"Anthropic API"|"OpenAI API"/);
  const texts = [];
  renderModelCards(container(), cardViews(ROWS, MINE), fakeDocument(texts));
  for (const text of texts) assert.doesNotMatch(text, /Protocol|^(Anthropic|OpenAI) API$/);
});

test("each card says which job slots use it — the workspace's and your own", () => {
  const views = cardViews(ROWS, MINE);
  const by = (id) => views.find((v) => v.id === id);
  assert.deepEqual(by("anthropic").slots.map((s) => s.role), ["main"]);
  assert.equal(by("anthropic").usage, "Used by Main");
  assert.deepEqual(by("chatgpt-subscription").slots, [{ role: "triage", label: "Triage", mine: true, model: "gpt-5.5" }]);
  assert.equal(by("chatgpt-subscription").usage, "Used by Triage (you)");
  // A catalog provider with no card of its own shows on Custom.
  assert.deepEqual(by("custom").slots.map((s) => s.role), ["research"]);
  assert.equal(by("openai").connected, false);
  // Your runner for a role wins over the workspace's connection for it.
  const both = cardViews(ROWS, [{ role: "main", agent: "claude", model: "opus", settings: "isolated" }]);
  assert.equal(both.find((v) => v.id === "anthropic").connected, false);
  assert.deepEqual(both.find((v) => v.id === "claude-subscription").slots.map((s) => s.role), ["main"]);
  // The Coding Plan runner shows on Z.AI.
  assert.equal(runnerCardFor({ agent: "claude", settings: "user" }).id, "zai");
  assert.equal(runnerCardFor({ agent: "claude", settings: "isolated" }).id, "claude-subscription");
  assert.equal(cardForConnection({ provider: "zhipu-glm-en" }).id, "zai");
});

test("the grid is one button per card, built from elements and text only", () => {
  const texts = [];
  const doc = fakeDocument(texts);
  const box = container();
  renderModelCards(box, cardViews(ROWS, MINE), doc);
  assert.equal(box.children.length, CARDS.length);
  const buttons = doc.made.filter((el) => el.tag === "button");
  assert.deepEqual(buttons.map((b) => b.attrs["data-card"]), CARDS.map((c) => c.id));
  for (const button of buttons) {
    assert.equal(button.type, "button");
    assert.equal(button.attrs["aria-haspopup"], "dialog");
  }
  const on = buttons.filter((b) => b.className.includes("mcard--on")).map((b) => b.attrs["data-card"]);
  assert.deepEqual(on, ["anthropic", "chatgpt-subscription", "custom"]);
  const pills = doc.made.filter((el) => el.className === "pill").map((p) => p.textContent);
  assert.deepEqual(pills, ["Main", "Triage", "Research"]);
  assert.ok(texts.includes("Subscription (runs on your Mac)"));
  assert.ok(texts.includes("Coding plan"));
});

test("the cards are square, two across at phone width, up to four", () => {
  assert.match(CSS, /\.mcard \{[^}]*aspect-ratio: 1 \/ 1;/);
  assert.match(CSS, /\.mcards \{[^}]*grid-template-columns: repeat\(2, minmax\(0, 1fr\)\);/);
  assert.match(CSS, /@container \(min-width: 34rem\) \{\s*\.mcards \{\s*grid-template-columns: repeat\(3,/);
  assert.match(CSS, /@container \(min-width: 46rem\) \{\s*\.mcards \{\s*grid-template-columns: repeat\(4,/);
  for (const id of ["main-model", "model-cards", "model-sheet", "model-select", "model-role", "runner-model", "coding-plan"]) {
    assert.ok(HTML.includes(`id="${id}"`), id);
  }
});

test("model pickers: listed, else curated, else the catalog — and the current one kept", () => {
  const anthropic = cardInfo("anthropic");
  assert.deepEqual(modelOptions(anthropic, null).map((m) => m.id), anthropic.models.map((m) => m.id));
  const listed = { provider: "anthropic", models: [{ id: "claude-new", context: 2000000 }] };
  assert.deepEqual(modelOptions(anthropic, listed).map((m) => m.id), ["claude-new"]);
  // A list for another provider is not this card's.
  assert.deepEqual(modelOptions(anthropic, { provider: "openai", models: ["x"] }).length, anthropic.models.length);
  // A catalog provider without a card offers the catalog's own list.
  const minimaxCn = providerInfo("minimax");
  assert.deepEqual(modelOptions(null, null, "minimax").map((m) => m.id), minimaxCn.models);
  assert.equal(modelOptions(anthropic, null, "anthropic", "my-pinned")[0].id, "my-pinned");
  assert.equal(formatContext(1000000), "1M");
  assert.equal(formatContext(1050000), "1.05M");
  assert.equal(formatContext(204800), "205K");
  assert.equal(formatContext(undefined), "");
  assert.equal(
    modelOptionLabel({ id: "x-1", label: "X One", context: 256000, price: "$1 / $2" }),
    "X One (x-1) · 256K context · $1 / $2 per M tokens",
  );
  assert.equal(modelOptionLabel({ id: "glm-5.3", label: "GLM-5.3" }), "GLM-5.3");
  assert.equal(modelOptionLabel("bare-id"), "bare-id");
});

test("the Main picker: the current model, switches on the same key, and your subscriptions", () => {
  const plain = mainModelChoices(ROWS, []);
  const current = plain.options.find((o) => o.value === plain.current);
  assert.deepEqual(current.action, { kind: "switch", model: "claude-opus-5-5", dropRunner: false });
  assert.ok(plain.options.some((o) => o.action.kind === "runner" && o.action.agent === "claude"));
  assert.ok(plain.options.some((o) => o.action.kind === "runner" && o.action.agent === "codex"));
  // With your own runner on Main, it is the current choice, and an API model
  // turns it off first.
  const withRunner = mainModelChoices(ROWS, [{ role: "main", agent: "claude", model: "opus", settings: "isolated" }]);
  const now = withRunner.options.find((o) => o.value === withRunner.current);
  assert.deepEqual(now.action, { kind: "runner", agent: "claude", settings: "isolated", model: "opus" });
  assert.ok(withRunner.options.filter((o) => o.action.kind === "switch").every((o) => o.action.dropRunner));
  // A server without the runner routes offers no runner choices.
  const old = mainModelChoices(ROWS, [], { runnersAvailable: false });
  assert.ok(old.options.every((o) => o.action.kind !== "runner"));
  // Nothing connected: the managed model is the current choice.
  const empty = mainModelChoices([], [], { runnersAvailable: false });
  assert.equal(empty.options.length, 1);
  assert.equal(empty.options[0].action.kind, "none");
  // A Coding Plan runner keeps its own route in the list.
  const plan = mainModelChoices([], [{ role: "main", agent: "claude", model: "glm-5.3", settings: "user" }]);
  const planNow = plan.options.find((o) => o.value === plan.current);
  assert.deepEqual(planNow.action, { kind: "runner", agent: "claude", settings: "user", model: "glm-5.3" });
});

test("a device runner is a choice, never a credential", () => {
  const claude = runnerValue(cardInfo("claude-subscription"), " sonnet ");
  assert.deepEqual(claude, { kind: "local_runner", agent: "claude", model: "sonnet", settings: "isolated" });
  assert.deepEqual(runnerValue(cardInfo("chatgpt-subscription"), "").agent, "codex");
  assert.deepEqual(runnerValue(cardInfo("zai"), "glm-5.3", { codingPlan: true }), {
    kind: "local_runner",
    agent: "claude",
    model: "glm-5.3",
    settings: "user",
  });
  assert.equal(runnerValue(cardInfo("openai"), "x"), null);
  const request = settingRequest("model-runner", "main", claude);
  assert.equal(request.method, "PUT");
  assert.equal(request.path, "/v1/models/mine/main");
  assert.equal(request.body.agent, "claude");
  // Beside the audit triple (`section`, `key`, `value`), nothing that could hold a secret.
  for (const field of Object.keys(request.body).filter((k) => !["section", "key", "value"].includes(k))) {
    assert.doesNotMatch(field, /key|token|secret|oauth|password/i);
  }
  assert.equal(settingRequest("model-runner", "main", null).method, "DELETE");
  const sw = settingRequest("model-switch", "main", { model: "claude-sonnet-5-5" });
  assert.equal(sw.method, "PATCH");
  assert.equal(sw.path, "/v1/models/main");
  assert.equal(sw.body.model, "claude-sonnet-5-5");
  assert.equal(settingRequest("model-runner", "a/../b", claude).path, "/v1/models/mine/a%2F..%2Fb");
  assert.equal(runnerCommand("codex"), "livingbrain jobs run --agent codex");
  assert.equal(runnerCommand("anything"), "livingbrain jobs run --agent claude");
});

test("a runner slot reads as yours, and last-seen reads as words", () => {
  const view = slotView([], "triage", MINE);
  assert.equal(view.runner, true);
  assert.equal(view.line, "ChatGPT subscription · gpt-5.5 · runs on your device");
  const now = Date.parse("2026-10-15T10:00:00Z");
  assert.equal(lastSeenText(null, now), "No device has picked up a job yet.");
  assert.equal(lastSeenText("2026-10-15T09:59:30Z", now), "Your device checked in just now.");
  assert.equal(lastSeenText("2026-10-15T09:30:00Z", now), "Your device last checked in 30 min ago.");
  assert.equal(lastSeenText("2026-10-15T05:00:00Z", now), "Your device last checked in 5 h ago.");
});

test("a provider with no model list never surfaces as a bare HTTP status", () => {
  assert.doesNotMatch(APP, /Could not list models/);
  assert.match(APP, /This provider doesn't publish a model list; showing known models\./);
});

/* --- Jev ------------------------------------------------------------- */

const JEV_DEPLOYMENT = {
  enabled: true,
  source: "deployment",
  deployment_key: true,
  workspace_key: null,
  model: "jev-1.13.0",
  default_model: "jev-1.13.0",
  custom_model: false,
  available: true,
};

test("the Jev card pins the adapter's model and is the last, optional card", () => {
  assert.equal(JEV_CARD.id, "jev");
  assert.equal(CARDS[CARDS.length - 1], JEV_CARD);
  const ADAPTER = readFileSync(
    new URL("../../crates/livingbrain-venture/src/lib.rs", import.meta.url),
    "utf8",
  );
  assert.match(ADAPTER, /cratefield_adapter_typesafe::DEFAULT_MODEL/);
  assert.equal(JEV_CARD.models[0].id, "jev-1.13.0");
  assert.match(JEV_PRIVACY, /redacted session digest/);
  assert.match(JEV_PRIVACY, /24,000 characters/);
  assert.match(JEV_PRIVACY, /US servers/);
  assert.match(JEV_PRIVACY, /classification only/);
});

test("Jev's three states: off, on the deployment's key, on the workspace's key", () => {
  const deployment = jevView(JEV_DEPLOYMENT);
  assert.equal(deployment.source, "deployment");
  assert.deepEqual(deployment.pill, { state: "live", label: "On · deployment key" });
  const workspace = jevView({ ...JEV_DEPLOYMENT, source: "workspace", workspace_key: "…9876" });
  assert.deepEqual(workspace.pill, { state: "live", label: "On · workspace key" });
  assert.equal(workspace.key, "••••9876");
  assert.match(workspace.line, /••••9876/);
  const off = jevView({ ...JEV_DEPLOYMENT, enabled: false, source: "off" });
  assert.deepEqual(off.pill, { state: "off", label: "Off" });
  assert.match(off.line, /switched off.*free triage rules still run/);
  const nokey = jevView({ ...JEV_DEPLOYMENT, source: "off", deployment_key: false });
  assert.match(nokey.line, /no TypeSafe key/);
  // Unknown state (an older server): the card shows no pill at all.
  const texts = [];
  const doc = fakeDocument(texts);
  renderModelCards(container(), cardViews([], [], null), doc);
  assert.ok(!doc.made.some((el) => el.className === "pill"));
  // Known: the Jev card wears its state, and only it.
  const known = [];
  const doc2 = fakeDocument(known);
  renderModelCards(container(), cardViews([], [], { ...JEV_DEPLOYMENT, source: "workspace", workspace_key: "…9876" }), doc2);
  const pills = doc2.made.filter((el) => el.className === "pill");
  assert.deepEqual(pills.map((p) => [p.textContent, p.attrs["data-state"]]), [["On · workspace key", "live"]]);
  const jevButton = doc2.made.find((el) => el.attrs["data-card"] === "jev");
  assert.ok(jevButton.className.includes("mcard--on"));
  assert.ok(known.includes("Optional"));
  // A key is never in the grid, only its last four.
  assert.ok(!known.some((t) => /WORKSPACESECRET/.test(t)));
});

test("Jev's save sends only what changed, and a key only when one was typed", () => {
  assert.deepEqual(jevChange({ enabled: true, apiKey: "", model: "jev-1.13.0" }, JEV_DEPLOYMENT).value, {});
  assert.deepEqual(jevChange({ enabled: false, apiKey: "", model: "jev-1.13.0" }, JEV_DEPLOYMENT).value, { enabled: false });
  assert.deepEqual(
    jevChange({ enabled: true, apiKey: "  ts-key-1234 ", model: "jev-2.0.0" }, JEV_DEPLOYMENT).value,
    { api_key: "ts-key-1234", model: "jev-2.0.0" },
  );
  // Back to the default model from a custom one.
  assert.deepEqual(
    jevChange({ enabled: true, model: "jev-1.13.0" }, { ...JEV_DEPLOYMENT, model: "jev-2.0.0", custom_model: true }).value,
    { model: "" },
  );
  const bad = jevChange({ enabled: true, apiKey: "two words", model: "jev 1; drop" }, JEV_DEPLOYMENT);
  assert.equal(bad.ok, false);
  assert.ok(bad.errors.apiKey && bad.errors.model);

  const put = settingRequest("jev", "settings", { enabled: false });
  assert.equal(put.method, "PUT");
  assert.equal(put.path, "/v1/models/jev");
  assert.equal(put.body.enabled, false);
  const remove = settingRequest("jev", "key", null);
  assert.equal(remove.method, "DELETE");
  assert.equal(remove.path, "/v1/models/jev/key");
  const probe = settingRequest("jev-test", "test", jevTestBody({ apiKey: "ts-x", model: "jev-1.13.0" }, JEV_DEPLOYMENT));
  assert.equal(probe.method, "POST");
  assert.equal(probe.path, "/v1/models/jev/test");
  assert.equal(probe.body.api_key, "ts-x");
  assert.equal(probe.body.model, undefined, "the saved model is not resent");
  assert.equal(jevTestMessage({ source: "workspace", model: "jev-1.13.0" }), "Jev answered on your key (jev-1.13.0).");
});

test("the Jev sheet is on the page, with the savers link and the privacy note", () => {
  for (const id of ["sheet-jev", "jev-enabled", "jev-key", "jev-model", "jev-save", "jev-test", "jev-remove-key", "jev-privacy"]) {
    assert.match(HTML, new RegExp(`id="${id}"`), id);
  }
  assert.match(HTML, /<input id="jev-key" type="password"/);
  assert.match(HTML, /href="#limits" id="jev-savers"/);
  assert.match(APP, /setSetting\("jev", "settings"/);
  assert.match(APP, /setSetting\("jev-test", "test"/);
});

/* --- a recording document -------------------------------------------- */

function fakeDocument(texts) {
  const made = [];
  const node = (tag) => {
    const el = {
      tag,
      className: "",
      type: "",
      attrs: {},
      children: [],
      _text: "",
      get textContent() {
        return this._text;
      },
      set textContent(value) {
        this._text = String(value);
        texts.push(this._text);
      },
      setAttribute(name, value) {
        this.attrs[name] = String(value);
      },
      appendChild(child) {
        this.children.push(child);
      },
    };
    made.push(el);
    return el;
  };
  return { made, createElement: node, createElementNS: (_ns, tag) => node(tag) };
}

function container() {
  return {
    children: [],
    replaceChildren() {
      this.children = [];
    },
    appendChild(c) {
      this.children.push(c);
    },
  };
}
