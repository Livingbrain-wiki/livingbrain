// Usage & limits — the pure half of the settings tab that turns the
// workspace's budgets (`GET /v1/settings/budgets`) into cards, and the
// cards' inputs back into one `PATCH`. Requests are described here and sent
// by `app.js` through the settings choke point (`LB.setting("budgets", …)`);
// the cards are painted with elements and text nodes only, as everywhere in
// `app/`.
//
// Every limit is the workspace's own when it set one, else the deployment's
// (its env var, else the built-in default). An input left empty means "use
// the default": saving it sends `null`, which clears the workspace's own.

/** The route both the read and the save use. */
export const BUDGETS_PATH = "/v1/settings/budgets";

/** `GET /v1/settings/budgets`. */
export function budgetsRequest() {
  return { method: "GET", path: BUDGETS_PATH };
}

/**
 * The numbers the tab shows, in order. `unit` decides how a value is read and
 * written: `usd` is entered in dollars and stored in cents.
 */
export const FIELDS = Object.freeze([
  {
    group: "coding_agents",
    key: "daily_tokens",
    label: "Tokens per day",
    hint: "Compile tokens the workspace may spend per UTC day.",
    unit: "tokens",
  },
  {
    group: "coding_agents",
    key: "monthly_tokens",
    label: "Tokens per month",
    hint: "Compile tokens per UTC month.",
    unit: "tokens",
  },
  {
    group: "coding_agents",
    key: "batch",
    label: "Sessions per run",
    hint: "Sessions one scheduled run (every 5 minutes) compiles.",
    unit: "sessions",
  },
  {
    group: "coding_agents",
    key: "job_tokens",
    label: "Per-session cap",
    hint: "The most tokens one session's compile may use, in and out.",
    unit: "tokens",
  },
  {
    group: "coding_agents",
    key: "concurrency",
    label: "Sessions at once",
    hint:
      "Sessions compiled side by side in one run (at most 6 run at once). A rate limit from your model halves it for a while.",
    unit: "sessions",
    advanced: true,
  },
  {
    group: "coding_agents",
    key: "max_chunks",
    label: "Calls per session",
    hint: "Model calls one session may take; the rest of a long one is left out.",
    unit: "calls",
    advanced: true,
  },
  {
    group: "coding_agents",
    key: "chunk_chars",
    label: "Characters per call",
    hint: "Transcript characters sent in one call.",
    unit: "chars",
    advanced: true,
  },
  {
    group: "coding_agents",
    key: "max_output_tokens",
    label: "Output per call",
    hint: "The longest answer one compile call may ask for, in tokens.",
    unit: "tokens",
    advanced: true,
  },
  {
    group: "ask",
    key: "context_chars",
    label: "Context size",
    hint: "The most characters of your pages one question sends to the model.",
    unit: "chars",
  },
  {
    group: "ask",
    key: "max_output_tokens",
    label: "Answer length",
    hint: "The longest answer, in tokens.",
    unit: "tokens",
  },
  {
    group: "spend",
    key: "monthly_cents",
    label: "Monthly spend limit",
    hint: "Past it, the brain stops calling the workspace's model until the month turns.",
    unit: "usd",
  },
]);

/**
 * Three starting points. Applying one fills the inputs; nothing is saved until
 * Save. Standard is the built-in defaults; High is what a busy team syncing
 * many sessions a day needs. The spend limit is money, so no preset moves it.
 */
export const PRESETS = Object.freeze([
  {
    id: "low",
    label: "Low",
    detail: "A light touch: a few sessions a day, short answers.",
    values: {
      "coding_agents.daily_tokens": 200_000,
      "coding_agents.monthly_tokens": 2_000_000,
      "coding_agents.batch": 1,
      "coding_agents.job_tokens": 40_000,
      "ask.context_chars": 50_000,
      "ask.max_output_tokens": 512,
    },
  },
  {
    id: "standard",
    label: "Standard",
    detail: "The defaults: steady compiling, room for a long question.",
    values: {
      "coding_agents.daily_tokens": 400_000,
      "coding_agents.monthly_tokens": 4_000_000,
      "coding_agents.batch": 1,
      "coding_agents.job_tokens": 80_000,
      "ask.context_chars": 200_000,
      "ask.max_output_tokens": 1024,
    },
  },
  {
    id: "high",
    label: "High",
    detail: "Every session compiled the same day, longer answers.",
    values: {
      "coding_agents.daily_tokens": 3_000_000,
      "coding_agents.monthly_tokens": 90_000_000,
      "coding_agents.batch": 3,
      "coding_agents.job_tokens": 200_000,
      "ask.context_chars": 400_000,
      "ask.max_output_tokens": 2048,
    },
  },
]);

/**
 * The triage strictness levels, as the select lists them. The server answers
 * with its own `options`; these are the labels.
 */
export const STRICTNESS = Object.freeze([
  { id: "lenient", label: "Lenient — only empty sessions skipped, only failing loops summarised" },
  { id: "balanced", label: "Balanced — also short lookups that changed nothing (default)" },
  { id: "strict", label: "Strict — any session that changed no file and said little" },
]);

/** The name an input carries: `group.key`. */
export function fieldName(field) {
  return `${field.group}.${field.key}`;
}

function number(value) {
  const n = Number(value);
  return Number.isFinite(n) ? n : 0;
}

/** A count in words a person reads at a glance: 3M, 400k, 1.5M, 900. */
export function formatCount(value) {
  const n = number(value);
  const trim = (x) => String(Number(x.toFixed(1)));
  if (Math.abs(n) >= 1_000_000_000) return `${trim(n / 1_000_000_000)}B`;
  if (Math.abs(n) >= 1_000_000) return `${trim(n / 1_000_000)}M`;
  if (Math.abs(n) >= 10_000) return `${trim(n / 1_000)}k`;
  return String(Math.round(n));
}

/** Cents as dollars: `$3.00`. */
export function formatUsd(cents) {
  return `$${(number(cents) / 100).toFixed(2)}`;
}

/** A field's value as a person reads it, with its unit. */
export function formatValue(field, value) {
  if (field.unit === "usd") return formatUsd(value);
  const count = formatCount(value);
  if (field.unit === "sessions") return `${count} ${number(value) === 1 ? "session" : "sessions"}`;
  if (field.unit === "calls") return `${count} ${number(value) === 1 ? "call" : "calls"}`;
  if (field.unit === "chars") return `${count} characters`;
  return `${count} tokens`;
}

/** What an input holds for a value: dollars for money, the number otherwise. */
export function inputValue(field, value) {
  if (value == null) return "";
  return field.unit === "usd" ? (number(value) / 100).toFixed(2) : String(value);
}

/**
 * The read, as the tab paints it: one entry per field with its value, where it
 * comes from, the deployment's value and the range; the pause switch; the
 * usage bars; and whether the reader may change anything.
 */
export function budgetView(response) {
  const body = response && typeof response === "object" ? response : {};
  const settings = body.settings || {};
  const fields = FIELDS.map((field) => {
    const entry = (settings[field.group] || {})[field.key] || {};
    return {
      ...field,
      name: fieldName(field),
      value: entry.value == null ? null : number(entry.value),
      source: entry.source === "workspace" ? "workspace" : "default",
      inherited: entry.inherited == null ? null : number(entry.inherited),
      min: entry.min == null ? null : number(entry.min),
      max: entry.max == null ? null : number(entry.max),
    };
  });
  const pausedEntry = (settings.coding_agents || {}).paused || {};
  const compiler = settings.coding_agents || {};
  const switchOf = (entry, fallback) => ({
    value: entry.value == null ? fallback : entry.value === true,
    source: entry.source === "workspace" ? "workspace" : "default",
    inherited: entry.inherited == null ? fallback : entry.inherited === true,
  });
  const strictnessEntry = compiler.triage_strictness || {};
  const strictnessOptions = Array.isArray(strictnessEntry.options)
    ? strictnessEntry.options.map(String)
    : STRICTNESS.map((s) => s.id);
  const usage = body.usage || {};
  const value = (name) => (fields.find((f) => f.name === name) || {}).value;
  return {
    editable: body.editable === true,
    role: String(body.role || ""),
    fields,
    paused: {
      value: pausedEntry.value === true,
      source: pausedEntry.source === "workspace" ? "workspace" : "default",
      inherited: pausedEntry.inherited === true,
    },
    savers: {
      triage: switchOf(compiler.triage || {}, true),
      compaction: switchOf(compiler.compaction || {}, true),
      strictness: {
        value: String(strictnessEntry.value || "balanced"),
        source: strictnessEntry.source === "workspace" ? "workspace" : "default",
        inherited: String(strictnessEntry.inherited || "balanced"),
        options: strictnessOptions,
      },
    },
    bars: [
      usageBar("Compiled today", usage.compile_tokens_today, value("coding_agents.daily_tokens"), formatCount, "tokens"),
      usageBar("Compiled this month", usage.compile_tokens_month, value("coding_agents.monthly_tokens"), formatCount, "tokens"),
      usageBar(
        "Spent this month",
        usage.spend_month_usd_micros == null ? null : number(usage.spend_month_usd_micros) / 10_000,
        value("spend.monthly_cents"),
        formatUsd,
        "",
      ),
    ],
    modelPriced: usage.spend_model_priced === true,
    updatedAt: String(body.updated_at || ""),
  };
}

/**
 * One usage bar: how much of `limit` is spent, as a fraction for the bar's
 * width (capped at 1) and a sentence for its label. A limit of zero is full
 * as soon as anything is spent; an unknown spend is an empty, labelled bar.
 */
export function usageBar(label, spent, limit, format, unit) {
  const known = spent != null;
  const used = known ? number(spent) : 0;
  const cap = limit == null ? 0 : number(limit);
  const fraction = cap > 0 ? Math.min(used / cap, 1) : used > 0 ? 1 : 0;
  const over = cap > 0 ? used >= cap : used > 0;
  const tone = over ? "over" : fraction >= 0.8 ? "near" : "ok";
  const suffix = unit ? ` ${unit}` : "";
  const text = known
    ? `${format(used)} of ${format(cap)}${suffix}`
    : `Not known yet (of ${format(cap)}${suffix})`;
  return { label, spent: used, limit: cap, fraction, tone, text, known };
}

/** The preset the current inputs match, or `custom`. */
export function matchPreset(inputs) {
  for (const preset of PRESETS) {
    const same = Object.entries(preset.values).every(([name, value]) => {
      const raw = inputs && inputs[name] != null ? String(inputs[name]).trim() : "";
      return raw !== "" && Number(raw) === value;
    });
    if (same) return preset.id;
  }
  return "custom";
}

/** The inputs a preset fills in, by field name. */
export function presetInputs(id) {
  const preset = PRESETS.find((p) => p.id === id);
  if (!preset) return null;
  const out = {};
  for (const [name, value] of Object.entries(preset.values)) out[name] = String(value);
  return out;
}

/** A refusal for one field: the field's name and a sentence for its error line. */
export class FieldError extends Error {
  constructor(name, message) {
    super(message);
    this.name = "FieldError";
    this.field = name;
  }
}

function parseInput(field, raw) {
  if (field.unit === "usd") {
    if (!/^\d+(\.\d{1,2})?$/.test(raw)) {
      throw new FieldError(fieldName(field), "Enter an amount in dollars, like 25 or 4.50.");
    }
    return Math.round(Number(raw) * 100);
  }
  if (!/^\d+$/.test(raw)) {
    throw new FieldError(fieldName(field), "Enter a whole number.");
  }
  return Number(raw);
}

/**
 * The `PATCH` body for what changed: `inputs` maps a field name to the text in
 * its input, `paused` is the switch, `savers` the token savers
 * (`{triage, compaction, strictness}`). A field whose input is empty goes back to
 * the default (`null`) when the workspace had set it; a value equal to what
 * is already in force is left out, so saving untouched cards pins nothing.
 * Throws a {@link FieldError} for a value that is not a number or is outside
 * the field's range. Answers `null` when nothing changed.
 */
export function budgetPatch(view, inputs, paused, savers) {
  const patch = {};
  const put = (group, key, value) => {
    patch[group] = patch[group] || {};
    patch[group][key] = value;
  };
  for (const field of view.fields) {
    const raw = inputs && inputs[field.name] != null ? String(inputs[field.name]).trim() : "";
    if (raw === "") {
      if (field.source === "workspace") put(field.group, field.key, null);
      continue;
    }
    const value = parseInput(field, raw);
    if (field.min != null && value < field.min) {
      throw new FieldError(field.name, `The smallest allowed is ${inputValue(field, field.min)}.`);
    }
    if (field.max != null && value > field.max) {
      throw new FieldError(field.name, `The largest allowed is ${inputValue(field, field.max)}.`);
    }
    if (value === field.value) continue;
    put(field.group, field.key, value);
  }
  if (typeof paused === "boolean" && paused !== view.paused.value) {
    // Back to the deployment's (running) is a clear, not a pinned `false`.
    put("coding_agents", "paused", paused === view.paused.inherited ? null : paused);
  }
  // The token savers: a switch or the strictness moved back to the
  // deployment's value is a clear, not a pin.
  const now = (savers && typeof savers === "object" ? savers : {});
  const shown = view.savers || {};
  for (const key of ["triage", "compaction"]) {
    const entry = shown[key];
    if (entry && typeof now[key] === "boolean" && now[key] !== entry.value) {
      put("coding_agents", key, now[key] === entry.inherited ? null : now[key]);
    }
  }
  const strictness = shown.strictness;
  if (strictness && typeof now.strictness === "string" && now.strictness !== strictness.value) {
    if (!strictness.options.includes(now.strictness)) {
      throw new FieldError("coding_agents.triage_strictness", "Pick one of the listed levels.");
    }
    put(
      "coding_agents",
      "triage_strictness",
      now.strictness === strictness.inherited ? null : now.strictness,
    );
  }
  return Object.keys(patch).length ? patch : null;
}

/** What the notice says when the read or the save fails. */
export function budgetErrorMessage(error, what) {
  if (error && error.status === 401) return "You are signed out — sign in again to see the limits.";
  if (error && error.status === 403) return `You can't ${what}: only owners and admins can change limits.`;
  const detail = error && error.body && error.body.detail;
  if (error && error.status === 422 && detail) return `Not saved: ${detail}.`;
  return `Could not ${what}: ${(error && error.message) || String(error)}`;
}

/* ------------------------------------------------------------------ paint */

function el(doc, tag, className, text) {
  const node = doc.createElement(tag);
  if (className) node.className = className;
  if (text != null) node.textContent = text;
  return node;
}

/** One usage bar: the label, the track and its fill, the sentence. */
function paintBar(doc, bar) {
  const wrap = el(doc, "div", "usebar");
  wrap.setAttribute("data-tone", bar.tone);
  const head = el(doc, "div", "usebar__head");
  head.appendChild(el(doc, "span", "usebar__label", bar.label));
  head.appendChild(el(doc, "span", "usebar__text", bar.text));
  wrap.appendChild(head);
  const track = el(doc, "div", "usebar__track");
  track.setAttribute("role", "meter");
  track.setAttribute("aria-label", bar.label);
  track.setAttribute("aria-valuemin", "0");
  track.setAttribute("aria-valuemax", String(bar.limit));
  track.setAttribute("aria-valuenow", String(Math.min(bar.spent, bar.limit)));
  track.setAttribute("aria-valuetext", bar.text);
  const fill = el(doc, "span", "usebar__fill");
  fill.style.width = `${Math.round(bar.fraction * 1000) / 10}%`;
  track.appendChild(fill);
  wrap.appendChild(track);
  return wrap;
}

/** One number input with its label, hint, default line and error line. */
function paintField(doc, field, editable) {
  const wrap = el(doc, "div", "field");
  const id = `budget-${field.group}-${field.key}`.replace(/_/g, "-");
  const label = el(doc, "label", null, field.label);
  label.setAttribute("for", id);
  wrap.appendChild(label);
  const input = doc.createElement("input");
  input.id = id;
  input.type = "text";
  input.setAttribute("inputmode", field.unit === "usd" ? "decimal" : "numeric");
  input.setAttribute("autocomplete", "off");
  input.setAttribute("data-budget", field.name);
  input.value = field.source === "workspace" ? inputValue(field, field.value) : "";
  input.placeholder = inputValue(field, field.inherited);
  input.disabled = !editable;
  const hintId = `${id}-hint`;
  const errId = `${id}-err`;
  input.setAttribute("aria-describedby", `${hintId} ${errId}`);
  wrap.appendChild(input);
  const hint = el(doc, "p", "hint", field.hint);
  hint.id = hintId;
  const source =
    field.source === "workspace"
      ? ` Set for this workspace; the default is ${formatValue(field, field.inherited)}.`
      : ` Using the default, ${formatValue(field, field.inherited)}.`;
  hint.appendChild(doc.createTextNode(source));
  wrap.appendChild(hint);
  const err = el(doc, "p", "field-error");
  err.id = errId;
  err.setAttribute("data-budget-error", field.name);
  wrap.appendChild(err);
  return wrap;
}

/**
 * The token savers: triage and compaction switches and the triage
 * strictness, with a line saying what each does and whose value it is.
 */
function paintSavers(doc, view) {
  const savers = view.savers;
  const wrap = el(doc, "fieldset", "budget-savers");
  wrap.appendChild(el(doc, "legend", null, "Token savers"));
  const check = (id, on, text) => {
    const label = el(doc, "label", "check");
    const box = doc.createElement("input");
    box.type = "checkbox";
    box.id = id;
    box.checked = on;
    box.disabled = !view.editable;
    label.appendChild(box);
    label.appendChild(doc.createTextNode(` ${text}`));
    return label;
  };
  wrap.appendChild(
    check(
      "budget-triage",
      savers.triage.value,
      "Triage — skip trivial sessions and log failing loops in one line, without a model call",
    ),
  );
  wrap.appendChild(
    check(
      "budget-compaction",
      savers.compaction.value,
      "Compaction — send the model the errors, commands and files, not the full tool output",
    ),
  );
  const field = el(doc, "div", "field");
  const label = el(doc, "label", null, "Triage strictness");
  label.setAttribute("for", "budget-strictness");
  field.appendChild(label);
  const select = doc.createElement("select");
  select.id = "budget-strictness";
  select.disabled = !view.editable;
  for (const id of savers.strictness.options) {
    const option = doc.createElement("option");
    option.value = id;
    option.textContent = (STRICTNESS.find((s) => s.id === id) || { label: id }).label;
    if (id === savers.strictness.value) option.selected = true;
    select.appendChild(option);
  }
  field.appendChild(select);
  field.appendChild(
    el(
      doc,
      "p",
      "hint",
      "Nothing is lost either way: a skipped or summarised session keeps its source, and " +
        "“Compile fully” on the queue compiles it in full.",
    ),
  );
  const err = el(doc, "p", "field-error");
  err.setAttribute("data-budget-error", "coding_agents.triage_strictness");
  field.appendChild(err);
  wrap.appendChild(field);
  return wrap;
}

/** The token savers as the inputs under `root` now say. */
export function readSavers(root) {
  if (!root) return {};
  const box = (id) => {
    const input = root.querySelector(`#${id}`);
    return input ? input.checked === true : undefined;
  };
  const select = root.querySelector("#budget-strictness");
  return {
    triage: box("budget-triage"),
    compaction: box("budget-compaction"),
    strictness: select ? String(select.value) : undefined,
  };
}

/** A card: a heading, a line under it, then its fields. */
function paintCard(doc, title, lede, children) {
  const card = el(doc, "div", "slot budget-card");
  card.appendChild(el(doc, "h3", "budget-card__title", title));
  if (lede) card.appendChild(el(doc, "p", "hint", lede));
  for (const child of children) card.appendChild(child);
  return card;
}

/**
 * Paints the tab into `root`: the presets (owners and admins only), the
 * Coding agents card with its pause switch and two usage bars, the Ask card,
 * and the spend limit with its bar. A member sees every value with the inputs
 * disabled.
 */
export function renderBudgets(root, view, doc = typeof document === "undefined" ? null : document) {
  if (!root || !doc) return root;
  root.replaceChildren();
  const fields = (group, advanced) =>
    view.fields.filter((f) => f.group === group && Boolean(f.advanced) === advanced);
  const bar = (label) => view.bars.find((b) => b.label === label);

  if (view.editable) {
    const presets = el(doc, "fieldset", "choice budget-presets");
    presets.appendChild(el(doc, "legend", null, "Start from"));
    const grid = el(doc, "div", "choice__grid");
    const options = [
      ...PRESETS,
      { id: "custom", label: "Custom", detail: "Your own numbers, below." },
    ];
    for (const option of options) {
      const label = el(doc, "label", "choice__card");
      const input = doc.createElement("input");
      input.type = "radio";
      input.name = "budget-preset";
      input.value = option.id;
      label.appendChild(input);
      const text = el(doc, "span", "choice__text");
      text.appendChild(el(doc, "strong", null, option.label));
      text.appendChild(el(doc, "span", null, option.detail));
      label.appendChild(text);
      grid.appendChild(label);
    }
    presets.appendChild(grid);
    presets.appendChild(
      el(doc, "p", "hint", "A preset fills the numbers below; nothing changes until you save."),
    );
    root.appendChild(presets);
  }

  // Coding agents.
  const pause = el(doc, "label", "check");
  const box = doc.createElement("input");
  box.type = "checkbox";
  box.id = "budget-paused";
  box.checked = view.paused.value;
  box.disabled = !view.editable;
  pause.appendChild(box);
  pause.appendChild(
    doc.createTextNode(" Pause compiling — synced sessions wait in the queue until you resume"),
  );
  const savers = paintSavers(doc, view);
  const advanced = el(doc, "details", "disclosure budget-advanced");
  advanced.appendChild(el(doc, "summary", null, "Advanced: the size of one compile, and how many run at once"));
  for (const field of fields("coding_agents", true)) advanced.appendChild(paintField(doc, field, view.editable));
  root.appendChild(
    paintCard(
      doc,
      "Coding agents",
      "Synced sessions compiled into your wiki, on the workspace's model.",
      [
        pause,
        paintBar(doc, bar("Compiled today")),
        paintBar(doc, bar("Compiled this month")),
        ...fields("coding_agents", false).map((f) => paintField(doc, f, view.editable)),
        savers,
        advanced,
      ],
    ),
  );

  // Ask.
  root.appendChild(
    paintCard(
      doc,
      "Ask",
      "Questions answered from your pages, on the workspace's model.",
      fields("ask", false).map((f) => paintField(doc, f, view.editable)),
    ),
  );

  // Spend.
  const spendBar = bar("Spent this month");
  const spendNote = view.modelPriced
    ? "Model tokens priced at the provider's list price, plus mail."
    : "Your model is not in the price sheet, so only mail is counted here.";
  root.appendChild(
    paintCard(doc, "Monthly spend", spendNote, [
      paintBar(doc, spendBar),
      ...fields("spend", false).map((f) => paintField(doc, f, view.editable)),
    ]),
  );
  return root;
}

/**
 * The text in every budget input under `root`, by field name. With
 * `effective`, an empty input reads as its placeholder — the default in force
 * — which is what a preset is matched against.
 */
export function readInputs(root, effective = false) {
  const out = {};
  if (!root) return out;
  for (const input of root.querySelectorAll("[data-budget]")) {
    const value = String(input.value || "").trim();
    out[input.getAttribute("data-budget")] =
      effective && value === "" ? String(input.placeholder || "") : value;
  }
  return out;
}
