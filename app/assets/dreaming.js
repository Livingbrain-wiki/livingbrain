// The Dreaming page (`dreaming.html`): a workspace's switch for the brain's
// tidying pass, the model it calls, the quick rules, "Run now", its status
// and the log of what it changed (`GET /v1/dreaming`, `PUT /v1/dreaming`,
// `GET /v1/dreaming/decisions`, `POST /v1/dreaming/run`).
//
// The pure helpers at the top are what the tests pin: the words the log
// speaks in, the status line, the settings a save sends, and the model
// choices. `dreamingPage(api)` is the DOM around them, built with
// `createElement` and text nodes like the rest of `app/`. The copy never says
// "scope", "cursor", "op" or "supersede": a change is "replaced", "ended",
// "removed" or "renamed".

/** Names a model slot in the picker. */
const SLOT_NAMES = Object.freeze({ main: "Main model", triage: "Triage model", research: "Research model" });

/** Why the pass refused a change, in the words the log shows (after "Skipped"). */
const REFUSALS = Object.freeze({
  "not-older": "the older fact must start first",
  "bad-date": "the date was not understood",
  "already-acted": "already changed earlier in this pass",
  "not-live": "it is no longer current",
  "unknown-id": "it was not among the facts reviewed",
  "same-fact": "it named the same fact twice",
  "not-in-memory": "it was already gone",
  "store-refused": "it could not be saved",
  "bad-attribute": "the new name is not allowed",
});

/** How many rows one page of the log asks for. */
export const PAGE_SIZE = 20;

/** A snake_case fact key as words: `auth_provider_decision` → `Auth provider decision`. */
export function humanKey(attribute) {
  const words = String(attribute || "")
    .replace(/[_-]+/g, " ")
    .trim();
  if (!words) return "a fact";
  return words.charAt(0).toUpperCase() + words.slice(1);
}

/** How long ago an RFC 3339 instant was, as the page says it. A date older
 * than a week is the date itself. */
export function ago(iso, now = Date.now()) {
  const then = Date.parse(String(iso || ""));
  if (Number.isNaN(then)) return "";
  const seconds = Math.max(0, Math.round((now - then) / 1000));
  if (seconds < 60) return "just now";
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes} min ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours} h ago`;
  const days = Math.round(hours / 24);
  if (days < 7) return days === 1 ? "1 day ago" : `${days} days ago`;
  return new Date(then).toISOString().slice(0, 10);
}

/** `1 subject` / `2 subjects`. */
function count(n, one, many) {
  return `${n} ${n === 1 ? one : many}`;
}

/**
 * One line of the log, as `{ muted, parts, detail, subject, when }`. `parts`
 * is the sentence: each `{ text }` or `{ text, strong: true }`, so the page
 * can bold the names without parsing markup. `muted` marks a change the pass
 * refused; `detail` is the values the change moved between, when it had any.
 */
export function describeDecision(row, now = Date.now()) {
  const fact = row.fact || null;
  const by = row.by || null;
  const name = humanKey(fact && fact.attribute);
  const when = ago(row.at, now);
  const subject = String(row.subject || "");
  const base = { subject, when };
  if (!row.applied) {
    const reason = REFUSALS[row.reason] || "it was not applied";
    return {
      ...base,
      muted: true,
      parts: [{ text: `Skipped ${name}: ${reason}` }],
      detail: "",
    };
  }
  switch (row.op) {
    case "supersede": {
      const parts = [
        { text: "Replaced " },
        { text: name, strong: true },
        { text: " with " },
        { text: by ? humanKey(by.attribute) : "a newer fact", strong: true },
      ];
      const detail =
        fact && by && fact.value && by.value ? `was ${fact.value}, now ${by.value}` : "";
      return { ...base, muted: false, parts, detail };
    }
    case "end": {
      const parts = [{ text: "Ended " }, { text: name, strong: true }];
      if (row.new_valid_to) parts.push({ text: ` from ${row.new_valid_to}` });
      return { ...base, muted: false, parts, detail: "" };
    }
    case "retract": {
      const parts = [{ text: "Removed " }, { text: name, strong: true }];
      const why = String(row.reason || "");
      if (why === "duplicate") parts.push({ text: " — a duplicate" });
      else if (why === "not-state" || why.startsWith("rule:")) parts.push({ text: " — a one-off note" });
      return { ...base, muted: false, parts, detail: "" };
    }
    case "rename": {
      const parts = [
        { text: "Renamed " },
        { text: name, strong: true },
        { text: " → " },
        { text: humanKey(row.new_attribute), strong: true },
      ];
      return { ...base, muted: false, parts, detail: "" };
    }
    default:
      return { ...base, muted: false, parts: [{ text: "Changed " }, { text: name, strong: true }], detail: "" };
  }
}

/**
 * The status strip's sentence: "Last ran 12 min ago · 2 subjects reviewed
 * this week · 37 changes this week", plus what is waiting.
 */
export function statusLine(status, now = Date.now()) {
  const s = status || {};
  const parts = [];
  parts.push(s.last_run_at ? `Last ran ${ago(s.last_run_at, now)}` : "Has not run yet");
  parts.push(`${count(Number(s.reviewed_week) || 0, "subject", "subjects")} reviewed this week`);
  const changes = Object.values(s.changes_week || {}).reduce((sum, n) => sum + (Number(n) || 0), 0);
  parts.push(`${count(changes, "change", "changes")} this week`);
  if (Number(s.queued) > 0) parts.push(`${s.queued} waiting`);
  if (s.run_requested) parts.push("a run is asked for the next tick");
  return parts.join(" · ");
}

/**
 * The body a save sends: only what differs from the page's settings. An
 * empty object is nothing to save.
 */
export function settingsPatch(current, next) {
  const patch = {};
  if (next.enabled !== current.enabled) patch.enabled = Boolean(next.enabled);
  if (next.rules_enabled !== current.rules_enabled) patch.rules_enabled = Boolean(next.rules_enabled);
  if (next.model && current.model && next.model !== current.model.role) patch.model = next.model;
  return patch;
}

/**
 * The model picker's options, one per job slot the workspace could call:
 * `{ value, label, disabled }`. A slot with no connection of its own is shown
 * but cannot be chosen.
 */
export function modelChoices(view) {
  return (view && view.models ? view.models : []).map((slot) => {
    const name = SLOT_NAMES[slot.role] || slot.role;
    const label = slot.usable ? `${name}: ${slot.model}` : `${name}: not connected`;
    return { value: slot.role, label, disabled: !slot.usable };
  });
}

/** The `GET /v1/dreaming/decisions` path for one page. */
export function logPath(before) {
  const query = new URLSearchParams({ limit: String(PAGE_SIZE) });
  if (before) query.set("before", before);
  return `/v1/dreaming/decisions?${query.toString()}`;
}

/* ----------------------------------------------------------------- the page */

const ENABLED_HINT = "When it is off, the brain keeps what it has and tidies nothing for this workspace.";
const RULES_HINT = "Drops one-off notes such as issue statuses and events, without a model call.";

/** Builds an element with text and attributes; never parses markup. */
function el(doc, tag, attrs = {}, children = []) {
  const node = doc.createElement(tag);
  for (const [key, value] of Object.entries(attrs)) {
    if (value === undefined || value === null || value === false) continue;
    if (key === "text") node.textContent = String(value);
    else if (key === "className") node.className = String(value);
    else if (key === "dataset") for (const [k, v] of Object.entries(value)) node.dataset[k] = String(v);
    else if (value === true) node.setAttribute(key, "");
    else node.setAttribute(key, String(value));
  }
  for (const child of children) if (child) node.appendChild(child);
  return node;
}

/** Wires the Dreaming page to the API. `api` is `app.js`'s `api`. */
export async function dreamingPage(api, doc = globalThis.document, now = () => Date.now()) {
  const $ = (id) => doc.getElementById(id);
  const form = $("dream-form");
  if (!form) return null;
  const enabled = $("dream-enabled");
  const model = $("dream-model");
  const rules = $("dream-rules");
  const save = $("dream-save");
  const run = $("dream-run");
  const readonly = $("dream-readonly");
  const strip = $("dream-strip");
  const log = $("dream-log");
  const empty = $("dream-log-empty");
  const more = $("dream-more");
  const notify = $("dream-notice");
  const enabledHint = $("dream-enabled-hint");
  const rulesHint = $("dream-rules-hint");
  const message = (text, tone = "warn") => {
    if (!notify) return;
    notify.textContent = text || "";
    if (text) notify.dataset.tone = tone;
    else delete notify.dataset.tone;
  };

  let view = null;
  let next = null;

  function paintSettings() {
    enabled.checked = Boolean(view.enabled);
    rules.checked = Boolean(view.rules_enabled);
    model.replaceChildren(
      ...modelChoices(view).map((choice) =>
        el(doc, "option", { value: choice.value, text: choice.label, disabled: choice.disabled }),
      ),
    );
    model.value = view.model ? view.model.role : "main";
    const editable = Boolean(view.editable);
    for (const control of [enabled, model, rules]) control.disabled = !editable;
    // A member reads the settings; the buttons that change them are not shown.
    save.hidden = !editable;
    run.hidden = !editable;
    readonly.hidden = editable;
    if (enabledHint) enabledHint.textContent = ENABLED_HINT;
    if (rulesHint) rulesHint.textContent = RULES_HINT;
  }

  function paintStatus() {
    const off = view.enabled ? "" : "Dreaming is off, so nothing is being tidied. ";
    strip.textContent = off + statusLine(view.status, now());
  }

  // Alias proposals dreaming made. An owner accepts (PUT) or dismisses
  // (DELETE) each; nothing is applied until accepted.
  function paintAliases() {
    const section = $("dream-aliases-section");
    const list = $("dream-aliases");
    if (!section || !list) return;
    const rows = Array.isArray(view.alias_suggestions) ? view.alias_suggestions : [];
    const items = rows.map((row) => {
      const children = [
        el(doc, "div", { className: "dream-item__what", text: `${row.alias} → ${row.canonical}` }),
        el(doc, "div", {
          className: "dream-item__meta",
          text: `${row.shared} shared attribute ${row.shared === 1 ? "family" : "families"}`,
        }),
      ];
      if (view.editable) {
        const accept = el(doc, "button", { type: "button", text: "Accept", dataset: { variant: "primary" } });
        const dismiss = el(doc, "button", { type: "button", text: "Dismiss" });
        accept.addEventListener("click", async () => {
          accept.disabled = true;
          try {
            await api("/v1/facts/aliases", { method: "PUT", body: { alias: row.alias, canonical: row.canonical } });
            message(`Accepted: ${row.alias} is filed under ${row.canonical}.`, "ok");
            await reload();
          } catch (error) {
            message(String(error && error.message ? error.message : error), "bad");
            accept.disabled = false;
          }
        });
        dismiss.addEventListener("click", async () => {
          dismiss.disabled = true;
          try {
            await api(`/v1/facts/aliases?alias=${encodeURIComponent(row.alias)}`, { method: "DELETE" });
            message(`Dismissed ${row.alias}.`, "ok");
            await reload();
          } catch (error) {
            message(String(error && error.message ? error.message : error), "bad");
            dismiss.disabled = false;
          }
        });
        children.push(el(doc, "div", { className: "form-actions" }, [accept, dismiss]));
      }
      return el(doc, "li", { className: "dream-item" }, children);
    });
    list.replaceChildren(...items);
    section.hidden = rows.length === 0;
  }

  function paintItem(row) {
    const line = describeDecision(row, now());
    const sentence = el(doc, "div", { className: "dream-item__what" }, line.parts.map((part) =>
      part.strong ? el(doc, "strong", { text: part.text }) : doc.createTextNode(part.text),
    ));
    const meta = [line.subject, line.when].filter(Boolean).join(" · ");
    const children = [sentence];
    if (line.detail) children.push(el(doc, "div", { className: "dream-item__detail", text: line.detail }));
    children.push(el(doc, "div", { className: "dream-item__meta", text: meta }));
    return el(
      doc,
      "li",
      { className: line.muted ? "dream-item is-muted" : "dream-item", dataset: { applied: line.muted ? "no" : "yes" } },
      children,
    );
  }

  async function loadLog(before) {
    const page = await api(logPath(before));
    const rows = Array.isArray(page.decisions) ? page.decisions : [];
    for (const row of rows) log.appendChild(paintItem(row));
    next = page.next || null;
    empty.hidden = log.children.length > 0;
    more.hidden = !next;
  }

  async function reload() {
    view = await api("/v1/dreaming");
    paintSettings();
    paintStatus();
    paintAliases();
  }

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const wanted = {
      enabled: enabled.checked,
      rules_enabled: rules.checked,
      model: model.value,
    };
    const patch = settingsPatch(view, wanted);
    if (!Object.keys(patch).length) {
      message("Nothing to save.", "ok");
      return;
    }
    save.disabled = true;
    try {
      view = await api("/v1/dreaming", { method: "PUT", body: patch });
      paintSettings();
      paintStatus();
      message("Saved.", "ok");
    } catch (error) {
      message(String(error && error.message ? error.message : error), "bad");
    } finally {
      save.disabled = !view.editable;
    }
  });

  run.addEventListener("click", async () => {
    run.disabled = true;
    try {
      await api("/v1/dreaming/run", { method: "POST" });
      message("Asked. The next run reads this workspace's facts.", "ok");
      await reload();
    } catch (error) {
      message(String(error && error.message ? error.message : error), "bad");
    } finally {
      run.disabled = !view.editable;
    }
  });

  more.addEventListener("click", async () => {
    if (!next) return;
    more.disabled = true;
    try {
      await loadLog(next);
    } catch (error) {
      message(String(error && error.message ? error.message : error), "bad");
    } finally {
      more.disabled = false;
    }
  });

  await reload();
  await loadLog(null);
  return { reload };
}
