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

/**
 * The roles, in the order the page shows them, with what each one is for.
 * `hint` is the one glance line on the slot card; `detail` is the fuller
 * description, kept as the card's title text.
 */
export const ROLES = Object.freeze([
  {
    id: "main",
    label: "Main",
    hint: "The strongest model you'll pay for.",
    detail: "Answers questions and writes pages. This is the model people talk to.",
  },
  {
    id: "triage",
    label: "Triage",
    hint: "Small and fast.",
    detail: "Decides quickly whether a message needs a reply. A small, fast model fits.",
  },
  {
    id: "research",
    label: "Research",
    hint: "Strong reasoning — can be slow.",
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
 * One job slot, as its card shows it: the role, its one-glance hint, and the
 * current state. A connected slot names the provider, the model and the masked
 * key; an unset one says what answers instead — Main, when Main is connected,
 * else the managed model. Named fields only: the API never returns a key, and
 * this copies nothing it does not name.
 */
export function slotView(rows, roleId) {
  const info = roleInfo(roleId) || {
    id: String(roleId || ""),
    label: String(roleId || "Unknown role"),
    hint: "",
    detail: "",
  };
  const list = Array.isArray(rows) ? rows : [];
  const row = list.find((candidate) => candidate && candidate.role === info.id);
  if (row) {
    const provider = providerForConnection(row);
    const model = String(row.model || "—");
    const key = maskedKey(row.key);
    return {
      role: info.id,
      roleLabel: info.label,
      roleHint: info.hint,
      roleDetail: info.detail,
      set: true,
      provider: provider.id,
      providerLabel: provider.name,
      model,
      key,
      status: statusView(row),
      line: `${provider.name} · ${model} · key ${key}`,
    };
  }
  const mainSet = info.id !== "main" && list.some((candidate) => candidate && candidate.role === "main");
  return {
    role: info.id,
    roleLabel: info.label,
    roleHint: info.hint,
    roleDetail: info.detail,
    set: false,
    provider: "",
    providerLabel: "",
    model: "",
    key: "",
    status: null,
    line: mainSet ? "Not set · uses Main" : "Not set · the managed model answers",
  };
}

/** The three slots, in role order, whatever order the API answered in. */
export function slotViews(rows) {
  return ROLES.map((role) => slotView(rows, role.id));
}

/**
 * Paints the three job slots. Each card's buttons carry `data-action`
 * (`connect`, `change` or `remove`) and `data-role`, which is how `app.js`
 * handles them with one listener on the container. A connected slot shows the
 * status pill only when the probe found something to say; a plain "works"
 * needs no badge.
 */
export function renderModelSlots(
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
  const button = (action, label, view) => {
    const node = el("button", "btn-sm", label);
    node.type = "button";
    node.setAttribute("data-action", action);
    node.setAttribute("data-role", view.role);
    node.setAttribute("aria-label", `${label} the ${view.roleLabel} model`);
    return node;
  };

  for (const view of list) {
    const card = el("article", "slot");
    card.setAttribute("aria-label", `${view.roleLabel} model`);

    const top = el("div", "slot__top");
    const head = el("div", "slot__head");
    head.appendChild(el("h3", "slot__role", view.roleLabel));
    const hint = el("p", "slot__hint", view.roleHint);
    if (view.roleDetail) hint.setAttribute("title", view.roleDetail);
    head.appendChild(hint);
    top.appendChild(head);
    if (view.set && view.status && view.status.state !== "live") {
      const pill = el("span", "pill", view.status.label);
      pill.setAttribute("data-state", view.status.state);
      top.appendChild(pill);
    }
    card.appendChild(top);

    card.appendChild(el("p", "slot__line", view.line));
    if (view.set && view.status && view.status.detail) {
      card.appendChild(el("p", "tiny", view.status.detail));
    }

    const actions = el("div", "slot__actions");
    if (view.set) {
      actions.appendChild(button("change", "Change", view));
      actions.appendChild(button("remove", "Remove", view));
    } else {
      actions.appendChild(button("connect", "Connect", view));
    }
    card.appendChild(actions);
    container.appendChild(card);
  }
  return container;
}
