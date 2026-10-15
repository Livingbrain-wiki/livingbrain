// Settings → Usage & limits: the read becomes cards with their source and
// usage, the inputs become a patch of only what changed (empty = back to the
// default), the presets fill and match, a member's cards are read-only, and
// everything is painted with elements and text nodes.

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

import {
  BUDGETS_PATH,
  FIELDS,
  PRESETS,
  FieldError,
  budgetsRequest,
  budgetView,
  budgetPatch,
  budgetErrorMessage,
  formatCount,
  formatUsd,
  matchPreset,
  presetInputs,
  readInputs,
  renderBudgets,
  usageBar,
} from "../assets/budgets.js";

/** What `GET /v1/settings/budgets` answers: production's env, one override. */
function response(overrides = {}) {
  const entry = (value, inherited, source = "default", min = 0, max = 1e12) => ({
    value,
    inherited,
    source,
    min,
    max,
  });
  return {
    workspace_id: "T0",
    role: "owner",
    editable: true,
    settings: {
      coding_agents: {
        paused: { value: false, inherited: false, source: "default", min: null, max: null },
        daily_tokens: entry(3_000_000, 3_000_000),
        monthly_tokens: entry(90_000_000, 90_000_000),
        batch: entry(5, 3, "workspace", 1, 20),
        job_tokens: entry(80_000, 80_000, "default", 1_000, 1_000_000),
        max_chunks: entry(4, 4, "default", 1, 20),
        chunk_chars: entry(32_000, 32_000, "default", 1_000, 200_000),
        max_output_tokens: entry(6_000, 6_000, "default", 256, 32_000),
        triage: { value: true, inherited: true, source: "default", min: null, max: null },
        compaction: { value: false, inherited: true, source: "workspace", min: null, max: null },
        triage_strictness: {
          value: "balanced",
          inherited: "balanced",
          source: "default",
          min: null,
          max: null,
          options: ["lenient", "balanced", "strict"],
        },
      },
      ask: {
        context_chars: entry(200_000, 200_000, "default", 2_000, 1_000_000),
        max_output_tokens: entry(1024, 1024, "default", 128, 8192),
      },
      spend: { monthly_cents: entry(300, 300, "default", 0, 100_000_000) },
    },
    usage: {
      compile_tokens_today: 2_400_000,
      compile_tokens_month: 9_000_000,
      spend_month_usd_micros: 1_500_000,
      spend_model_priced: true,
    },
    ...overrides,
  };
}

/** The inputs as the page holds them right after painting `view`. */
function inputsOf(view) {
  const out = {};
  for (const field of view.fields) {
    out[field.name] =
      field.source === "workspace"
        ? field.unit === "usd"
          ? (field.value / 100).toFixed(2)
          : String(field.value)
        : "";
  }
  return out;
}

test("the read is one GET on the settings path", () => {
  assert.deepEqual(budgetsRequest(), { method: "GET", path: "/v1/settings/budgets" });
  assert.equal(BUDGETS_PATH, "/v1/settings/budgets");
});

test("the view carries each value, its source, the default and the usage", () => {
  const view = budgetView(response());
  assert.equal(view.editable, true);
  const batch = view.fields.find((f) => f.name === "coding_agents.batch");
  assert.equal(batch.value, 5);
  assert.equal(batch.source, "workspace");
  assert.equal(batch.inherited, 3);
  assert.equal(batch.max, 20);
  const daily = view.fields.find((f) => f.name === "coding_agents.daily_tokens");
  assert.equal(daily.source, "default");
  const [today, month, spend] = view.bars;
  assert.equal(today.fraction, 0.8);
  assert.equal(today.tone, "near");
  assert.equal(today.text, "2.4M of 3M tokens");
  assert.equal(month.tone, "ok");
  // $1.50 of $3.00.
  assert.equal(spend.text, "$1.50 of $3.00");
  assert.equal(spend.fraction, 0.5);
  // Every field the server answers is shown; nothing extra.
  assert.equal(view.fields.length, FIELDS.length);
});

test("a garbage answer is an empty, read-only view rather than a crash", () => {
  const view = budgetView(null);
  assert.equal(view.editable, false);
  assert.equal(view.fields.every((f) => f.value === null), true);
  assert.equal(view.bars[2].known, false);
});

test("an untouched form saves nothing", () => {
  const view = budgetView(response());
  assert.equal(budgetPatch(view, inputsOf(view), false), null);
});

test("a changed number is sent; an emptied override goes back to the default", () => {
  const view = budgetView(response());
  const inputs = inputsOf(view);
  inputs["coding_agents.daily_tokens"] = "5000000";
  inputs["coding_agents.batch"] = "";
  inputs["spend.monthly_cents"] = "25";
  assert.deepEqual(budgetPatch(view, inputs, false), {
    coding_agents: { daily_tokens: 5_000_000, batch: null },
    spend: { monthly_cents: 2500 },
  });
});

test("typing the default in force pins nothing", () => {
  const view = budgetView(response());
  const inputs = inputsOf(view);
  inputs["coding_agents.daily_tokens"] = "3000000";
  assert.equal(budgetPatch(view, inputs, false), null);
});

test("pausing is sent; resuming to the default clears it", () => {
  const view = budgetView(response());
  assert.deepEqual(budgetPatch(view, inputsOf(view), true), {
    coding_agents: { paused: true },
  });
  const paused = budgetView(
    response({
      settings: {
        ...response().settings,
        coding_agents: {
          ...response().settings.coding_agents,
          paused: { value: true, inherited: false, source: "workspace" },
        },
      },
    }),
  );
  assert.deepEqual(budgetPatch(paused, inputsOf(paused), false), {
    coding_agents: { paused: null },
  });
});

test("a value outside the range or not a number names its field", () => {
  const view = budgetView(response());
  const refused = (name, value) => {
    const inputs = { ...inputsOf(view), [name]: value };
    try {
      budgetPatch(view, inputs, false);
    } catch (error) {
      assert.ok(error instanceof FieldError);
      return error;
    }
    assert.fail(`${name}=${value} was accepted`);
  };
  assert.equal(refused("coding_agents.batch", "21").field, "coding_agents.batch");
  assert.match(refused("coding_agents.batch", "21").message, /largest allowed is 20/);
  assert.match(refused("coding_agents.daily_tokens", "lots").message, /whole number/);
  assert.match(refused("coding_agents.daily_tokens", "1.5").message, /whole number/);
  assert.match(refused("ask.context_chars", "10").message, /smallest allowed is 2000/);
  assert.match(refused("spend.monthly_cents", "-3").message, /dollars/);
});

test("a preset fills its numbers and is recognised; anything else is custom", () => {
  const standard = presetInputs("standard");
  assert.equal(standard["coding_agents.daily_tokens"], "400000");
  assert.equal(matchPreset(standard), "standard");
  assert.equal(matchPreset(presetInputs("high")), "high");
  assert.equal(matchPreset({ ...standard, "coding_agents.batch": "2" }), "custom");
  assert.equal(presetInputs("nope"), null);
  // No preset moves the spend limit: it is money.
  for (const preset of PRESETS) {
    assert.ok(!("spend.monthly_cents" in preset.values), preset.id);
  }
  // Applying High over production's settings sends only what differs.
  const view = budgetView(response());
  const patch = budgetPatch(view, { ...inputsOf(view), ...presetInputs("high") }, false);
  assert.deepEqual(patch, {
    coding_agents: { batch: 3, job_tokens: 200_000 },
    ask: { context_chars: 400_000, max_output_tokens: 2048 },
  });
});

test("counts and money read at a glance", () => {
  assert.equal(formatCount(3_000_000), "3M");
  assert.equal(formatCount(400_000), "400k");
  assert.equal(formatCount(1_500_000), "1.5M");
  assert.equal(formatCount(900), "900");
  assert.equal(formatUsd(300), "$3.00");
  assert.equal(usageBar("x", 5, 0, formatCount, "tokens").tone, "over");
  assert.equal(usageBar("x", 0, 0, formatCount, "tokens").fraction, 0);
  assert.equal(usageBar("x", 50, 10, formatCount, "tokens").fraction, 1);
});

test("refusals are said plainly", () => {
  assert.match(budgetErrorMessage({ status: 403 }, "save the limits"), /owners and admins/);
  assert.match(
    budgetErrorMessage({ status: 422, body: { detail: "`coding_agents.batch` must be from 1 to 20" } }, "save"),
    /Not saved: `coding_agents.batch`/,
  );
});

/* -------------------------------------------------------------- painting */

function recorderDoc() {
  const make = (tag) => ({
    tag,
    className: "",
    textContent: "",
    value: "",
    placeholder: "",
    type: "",
    name: "",
    id: "",
    checked: false,
    disabled: false,
    style: {},
    attrs: {},
    children: [],
    setAttribute(name, value) {
      this.attrs[name] = value;
    },
    getAttribute(name) {
      return this.attrs[name];
    },
    appendChild(child) {
      this.children.push(child);
      return child;
    },
  });
  return {
    createElement: make,
    createTextNode: (text) => ({ tag: "#text", textContent: text, children: [] }),
  };
}

function container() {
  return {
    children: [],
    replaceChildren() {
      this.children = [];
    },
    appendChild(child) {
      this.children.push(child);
    },
  };
}

function walk(node, out = []) {
  out.push(node);
  for (const child of node.children || []) walk(child, out);
  return out;
}

test("an owner's cards have presets and editable inputs; a member's are read-only", () => {
  const doc = recorderDoc();
  const root = container();
  renderBudgets(root, budgetView(response()), doc);
  const all = walk(root);
  const inputs = all.filter((n) => n.tag === "input" && n.attrs["data-budget"]);
  assert.equal(inputs.length, FIELDS.length);
  assert.ok(inputs.every((n) => !n.disabled));
  const batch = inputs.find((n) => n.attrs["data-budget"] === "coding_agents.batch");
  assert.equal(batch.value, "5", "an override is in the input");
  const daily = inputs.find((n) => n.attrs["data-budget"] === "coding_agents.daily_tokens");
  assert.equal(daily.value, "", "a default is the placeholder, not a value");
  assert.equal(daily.placeholder, "3000000");
  assert.equal(all.filter((n) => n.attrs && n.attrs.role === "meter").length, 3);
  assert.equal(all.filter((n) => n.name === "budget-preset").length, PRESETS.length + 1);

  const memberRoot = container();
  renderBudgets(memberRoot, budgetView(response({ editable: false, role: "member" })), doc);
  const seen = walk(memberRoot);
  assert.ok(
    seen.filter((n) => n.tag === "input" && n.attrs["data-budget"]).every((n) => n.disabled),
  );
  assert.equal(seen.filter((n) => n.name === "budget-preset").length, 0, "no presets");
  assert.ok(seen.find((n) => n.id === "budget-paused").disabled);
});

test("a fill is a width, never markup, and the bar says its numbers", () => {
  const doc = recorderDoc();
  const root = container();
  renderBudgets(root, budgetView(response()), doc);
  const fills = walk(root).filter((n) => n.className === "usebar__fill");
  assert.deepEqual(
    fills.map((n) => n.style.width),
    ["80%", "10%", "50%"],
  );
  const meter = walk(root).find((n) => n.attrs && n.attrs.role === "meter");
  assert.equal(meter.attrs["aria-valuetext"], "2.4M of 3M tokens");
});

test("readInputs reads values, or the default in force when asked", () => {
  const fake = {
    querySelectorAll: () => [
      { value: "", placeholder: "400000", getAttribute: () => "coding_agents.daily_tokens" },
      { value: " 7 ", placeholder: "3", getAttribute: () => "coding_agents.batch" },
    ],
  };
  assert.deepEqual(readInputs(fake), {
    "coding_agents.daily_tokens": "",
    "coding_agents.batch": "7",
  });
  assert.deepEqual(readInputs(fake, true), {
    "coding_agents.daily_tokens": "400000",
    "coding_agents.batch": "7",
  });
});

test("no HTML is ever assigned in the budgets module", () => {
  const source = readFileSync(new URL("../assets/budgets.js", import.meta.url), "utf8");
  assert.doesNotMatch(source, /\.(inner|outer)HTML\s*=|insertAdjacentHTML\s*\(|document\.write\s*\(/);
});

test("the token savers read from the server and patch only what moved", () => {
  const view = budgetView(response());
  assert.deepEqual(view.savers.triage, { value: true, source: "default", inherited: true });
  assert.equal(view.savers.compaction.value, false);
  assert.equal(view.savers.compaction.source, "workspace");
  assert.deepEqual(view.savers.strictness.options, ["lenient", "balanced", "strict"]);
  const inputs = inputsOf(view);
  // Untouched: nothing to save.
  assert.equal(
    budgetPatch(view, inputs, undefined, { triage: true, compaction: false, strictness: "balanced" }),
    null,
  );
  // Triage off pins false; compaction back on is the default again, so it clears;
  // strict pins the name.
  assert.deepEqual(
    budgetPatch(view, inputs, undefined, { triage: false, compaction: true, strictness: "strict" }),
    { coding_agents: { triage: false, compaction: null, triage_strictness: "strict" } },
  );
  assert.throws(
    () => budgetPatch(view, inputs, undefined, { strictness: "max" }),
    (error) => error instanceof FieldError && error.field === "coding_agents.triage_strictness",
  );
  // An older server without the settings: both on, balanced.
  const old = budgetView({ settings: { coding_agents: {} } });
  assert.equal(old.savers.triage.value, true);
  assert.equal(old.savers.strictness.value, "balanced");
});

test("the token savers paint as two switches and a select, read-only for a member", () => {
  const doc = recorderDoc();
  const root = container();
  renderBudgets(root, budgetView(response()), doc);
  const all = walk(root);
  assert.equal(all.find((n) => n.id === "budget-triage").checked, true);
  assert.equal(all.find((n) => n.id === "budget-compaction").checked, false);
  const select = all.find((n) => n.id === "budget-strictness");
  assert.equal(select.tag, "select");
  assert.equal(all.filter((n) => n.tag === "option").length, 3);
  const memberRoot = container();
  renderBudgets(memberRoot, budgetView(response({ editable: false, role: "member" })), doc);
  const seen = walk(memberRoot);
  assert.ok(seen.find((n) => n.id === "budget-triage").disabled);
  assert.ok(seen.find((n) => n.id === "budget-strictness").disabled);
});
