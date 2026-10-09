// Model connections — the pure half of the settings page's Models section.
//
// The providers are the vendored catalog, `providers.json`: every provider
// Colonizer supports (synced by `scripts/sync-providers`), plus Living Brain's
// two built-ins, Anthropic and OpenAI. The server compiles in the same file
// (`crates/livingbrain-models/src/catalog.rs`), so a provider id means the same
// base URL, auth style and wire on both sides. `custom` is anything else, and
// names its own.
//
// Nothing here touches the DOM except the render functions, which build
// elements and text nodes only, as everywhere else in `app/`.

import catalog from "./providers.json" with { type: "json" };
import { providerMark } from "./marks.js";

/** The roles, in the order the page shows them, with what each one is for. */
export const ROLES = Object.freeze([
  {
    id: "main",
    label: "Main",
    detail: "Answers questions and writes pages. This is the model people talk to.",
  },
  {
    id: "triage",
    label: "Triage",
    detail: "Decides quickly whether a message needs a reply. A small, fast model fits.",
  },
  {
    id: "research",
    label: "Research",
    detail: "Longer, multi-step digging across sources. A strong reasoning model fits.",
  },
]);

/** Every catalog provider, built-ins first, then the synced list by name. */
export const PROVIDERS = Object.freeze([
  ...catalog.builtin,
  ...[...catalog.providers].sort((a, b) =>
    a.name.localeCompare(b.name, "en", { sensitivity: "base" }),
  ),
]);

/** Where the synced list came from, for the page's small print. */
export const CATALOG_SOURCE = catalog.source;

/** The ones most people want, shown first as cards. */
export const POPULAR = Object.freeze([
  "anthropic",
  "openai",
  "deepseek",
  "openrouter",
  "zhipu-glm-en",
  "minimax-en",
  "kimi",
  "qwencloud",
]);

/** Shorter names for the popular cards, where the catalog's is a mouthful. */
export const POPULAR_LABELS = Object.freeze({
  "zhipu-glm-en": "Z.AI",
  "minimax-en": "MiniMax",
  qwencloud: "Qwen",
});

/** The pseudo-entry for an endpoint that is not in the catalog. */
export const CUSTOM = Object.freeze({
  id: "custom",
  name: "Custom endpoint",
  base_url: "",
  auth: "bearer",
  wire: "openai",
  site: "",
});

/** The two wires, as the custom form offers them. */
export const WIRES = Object.freeze([
  { value: "anthropic", label: "Anthropic Messages", detail: "POST /v1/messages" },
  { value: "openai", label: "OpenAI Chat Completions", detail: "POST /v1/chat/completions" },
]);

/** The two ways a key travels. */
export const AUTHS = Object.freeze([
  { value: "bearer", label: "Bearer token", detail: "Authorization: Bearer <key>" },
  { value: "x-api-key", label: "x-api-key header", detail: "x-api-key: <key>" },
]);

/** One role by id, or undefined. */
export function roleInfo(id) {
  return ROLES.find((role) => role.id === id);
}

/** One provider by id: a catalog entry, `CUSTOM`, or undefined. */
export function providerInfo(id) {
  if (id === "custom") return CUSTOM;
  return PROVIDERS.find((provider) => provider.id === id);
}

/** The host a provider's endpoint is on, for the picker's second line. */
export function providerHost(provider) {
  try {
    return new URL(String(provider.base_url).replace(/\$\{\w+\}/g, "x")).host;
  } catch {
    return "";
  }
}

/**
 * Providers whose name, id or host contains every word of `query`, by name.
 * An empty query is the whole list.
 */
export function searchProviders(query, list = PROVIDERS) {
  const words = String(query || "")
    .toLowerCase()
    .split(/\s+/)
    .filter(Boolean);
  if (!words.length) return [...list];
  return list.filter((provider) => {
    const hay = `${provider.name} ${provider.id} ${providerHost(provider)}`.toLowerCase();
    return words.every((word) => hay.includes(word));
  });
}

/** `${NAME}` placeholders filled from `values`; an empty one stays as it is. */
export function fillTemplate(url, values = {}) {
  return String(url || "").replace(/\$\{(\w+)\}/g, (whole, name) => {
    const value = values[name] == null ? "" : String(values[name]).trim();
    return value || whole;
  });
}

/** The catalog entry a stored connection belongs to, or `CUSTOM`. */
export function providerForConnection(row) {
  return providerInfo(row && row.provider) || CUSTOM;
}

/** What the key hint says, from how the provider wants the key. */
export function keyHint(provider) {
  return provider && provider.auth === "x-api-key"
    ? "Sent in an x-api-key header."
    : "Sent as a Bearer token.";
}

/** A URL variable's value is a path or host fragment, nothing more. */
const PLAIN_VALUE = /^[A-Za-z0-9._~-]{1,128}$/;

/**
 * Checks the connect form. Answers `{ ok, errors, role, value }`: `errors`
 * maps a field (`role`, `provider`, `baseUrl`, `apiKey`, `model`, or
 * `var:<NAME>`) to the sentence shown under it, and `value` is the connection
 * `LB.setting("models", role, value)` sends when `ok`.
 *
 * The form reads: `role`, `provider`, `variables` ({NAME: value}),
 * `baseUrl` with `editUrl` (true when the member unlocked the endpoint),
 * `wire` and `auth` (custom only), `apiKey`, `model`.
 *
 * A catalog provider sends its id and variables and lets the server fill the
 * endpoint; only an edited endpoint, or a custom one, sends `base_url`. The
 * URL rule is the server's: https only, no credentials in it.
 */
export function validateConnect(form) {
  const f = form || {};
  const errors = {};
  const role = roleInfo(f.role);
  const provider = providerInfo(f.provider);
  if (!role) errors.role = "Choose what this model is for.";
  if (!provider) errors.provider = "Choose a provider.";

  const custom = provider === CUSTOM;
  const variables = {};
  for (const variable of (provider && provider.variables) || []) {
    const value = String((f.variables || {})[variable.name] || "").trim();
    if (!value) errors[`var:${variable.name}`] = `Enter the ${variable.label}.`;
    else if (!PLAIN_VALUE.test(value)) {
      errors[`var:${variable.name}`] = "Letters, digits, '-', '_', '.' or '~' only.";
    } else variables[variable.name] = value;
  }

  let baseUrl = null;
  if (provider && (custom || f.editUrl)) {
    const raw = String(f.baseUrl || "").trim();
    if (!raw) {
      errors.baseUrl = "Enter the endpoint's base URL.";
    } else {
      let parsed = null;
      try {
        parsed = new URL(raw);
      } catch {
        parsed = null;
      }
      if (!parsed || !parsed.hostname) {
        errors.baseUrl = "That is not a URL. It should look like https://api.example.com.";
      } else if (parsed.protocol !== "https:") {
        errors.baseUrl = "Use an https:// URL. Keys are never sent in clear text.";
      } else if (parsed.username || parsed.password) {
        errors.baseUrl = "Put the key in the API key field, not in the URL.";
      } else if (/\$\{\w+\}/.test(raw)) {
        errors.baseUrl = "Fill in the placeholder in the URL.";
      } else {
        baseUrl = raw.replace(/\/+$/, "");
      }
    }
  }

  const wire = custom ? (WIRES.some((w) => w.value === f.wire) ? f.wire : null) : null;
  const auth = custom ? (AUTHS.some((a) => a.value === f.auth) ? f.auth : null) : null;
  if (custom && !wire) errors.wire = "Choose the API this endpoint speaks.";
  if (custom && !auth) errors.auth = "Choose how the key is sent.";

  const apiKey = String(f.apiKey || "").trim();
  if (!apiKey) errors.apiKey = "Paste the API key for this provider.";

  const model = String(f.model || "").trim();
  if (!model) errors.model = "Enter the model name.";

  const ok = Object.keys(errors).length === 0;
  if (!ok) return { ok, errors, role: role ? role.id : null, value: null };
  const value = { provider: provider.id, api_key: apiKey, model, fallback_to_managed: false };
  if (baseUrl) value.base_url = baseUrl;
  if (Object.keys(variables).length && !baseUrl) value.variables = variables;
  if (custom) {
    value.wire = wire;
    value.auth = auth;
  }
  return { ok, errors, role: role.id, value };
}

/**
 * The body for `POST /v1/models/discover`: the same endpoint fields a connect
 * sends, and the key. The browser sends it to Living Brain's own API, which
 * asks the provider; the page never calls a provider itself.
 */
export function discoverRequest(value) {
  const body = { provider: value.provider, api_key: value.api_key };
  for (const key of ["base_url", "variables", "wire", "auth"]) {
    if (value[key] !== undefined) body[key] = value[key];
  }
  return { method: "POST", path: "/v1/models/discover", body };
}

/** How many model chips to show before the list is left to the search box. */
export const CHIP_LIMIT = 24;

/** The key as the page shows it: dots and the last four the API sent. */
export function maskedKey(key) {
  const last4 = String(key || "").replace(/^…/, "").slice(-4);
  return last4 ? `••••${last4}` : "••••";
}

/**
 * The status the probe recorded, as a pill: `works` is the green one, and
 * anything else is a warning that names what is missing.
 */
export function statusView(row) {
  const missing = Array.isArray(row && row.missing) ? row.missing.map(String) : [];
  if (row && row.status === "works") {
    return { state: "live", label: "Works", detail: "" };
  }
  return {
    state: "warn",
    label: row && row.status ? "Answers only" : "Not checked",
    detail: missing.length ? `Missing: ${missing.join(", ")}` : "",
  };
}

/**
 * One connected model, as its card shows it. Named fields only: the API never
 * returns a key, and this copies nothing it does not name.
 */
export function connectionView(row) {
  const role = roleInfo(row && row.role);
  const provider = providerForConnection(row);
  return {
    role: String((row && row.role) || ""),
    roleLabel: role ? role.label : String((row && row.role) || "Unknown role"),
    roleDetail: role ? role.detail : "",
    provider: provider.id,
    providerLabel: provider.name,
    model: String((row && row.model) || "—"),
    key: maskedKey(row && row.key),
    status: statusView(row),
  };
}

/** The list, in role order, whatever order the API answered in. */
export function connectionViews(rows) {
  const list = Array.isArray(rows) ? rows : [];
  const order = (row) => {
    const at = ROLES.findIndex((role) => role.id === (row && row.role));
    return at === -1 ? ROLES.length : at;
  };
  return [...list].sort((a, b) => order(a) - order(b)).map(connectionView);
}

/**
 * Paints the connected-model cards. Each card's two buttons carry
 * `data-action` and `data-role`, which is how `app.js` handles them with one
 * listener on the container.
 */
export function renderModelCards(
  container,
  views,
  doc = typeof document === "undefined" ? null : document,
) {
  if (!container || !doc) return container;
  container.replaceChildren();
  const list = Array.isArray(views) ? views : [];
  const el = (tag, className, text) => {
    const node = doc.createElement(tag);
    if (className) node.className = className;
    if (text != null) node.textContent = text;
    return node;
  };

  if (!list.length) {
    const empty = el("div", "empty");
    empty.appendChild(el("strong", null, "No models connected yet"));
    empty.appendChild(
      el("p", "muted", "Until you connect one, Living Brain answers with its managed model."),
    );
    container.appendChild(empty);
    return container;
  }

  for (const view of list) {
    const card = el("article", "conn");
    card.setAttribute("aria-label", `${view.roleLabel} model`);

    const head = el("div", "conn__head");
    const title = el("div", "conn__title");
    title.appendChild(el("span", "conn__role", view.roleLabel));
    title.appendChild(el("span", "conn__model", view.model));
    head.appendChild(title);
    const pill = el("span", "pill", view.status.label);
    pill.setAttribute("data-state", view.status.state);
    head.appendChild(pill);
    card.appendChild(head);

    const facts = el("dl", "conn__facts");
    for (const [term, value] of [
      ["Provider", view.providerLabel],
      ["Key", view.key],
    ]) {
      const pair = el("div");
      pair.appendChild(el("dt", null, term));
      const dd = el("dd");
      if (term === "Provider") dd.appendChild(providerMark(view.provider, value, doc));
      dd.appendChild(el("span", null, value));
      pair.appendChild(dd);
      facts.appendChild(pair);
    }
    card.appendChild(facts);
    if (view.status.detail) card.appendChild(el("p", "tiny", view.status.detail));

    const actions = el("div", "conn__actions");
    for (const [action, label] of [
      ["replace", "Replace"],
      ["disconnect", "Disconnect"],
    ]) {
      const button = el("button", "btn-sm", label);
      button.type = "button";
      button.setAttribute("data-action", action);
      button.setAttribute("data-role", view.role);
      button.setAttribute("aria-label", `${label} the ${view.roleLabel} model`);
      actions.appendChild(button);
    }
    card.appendChild(actions);
    container.appendChild(card);
  }
  return container;
}
