// Model connections — the pure half of the settings page's Models section.
//
// What the server accepts lives in `crates/livingbrain-models/src/handlers.rs`:
// three roles (`ROLES`) and five provider values (`resolve_base_url`). Four
// providers are presets whose base URL the server fixes itself; `custom` is any
// OpenAI-compatible endpoint, and needs `base_url`. Google and Groq have no
// server preset, but both serve an OpenAI-compatible API, so the page offers
// them as shortcuts that send `custom` with their endpoint filled in.
//
// Nothing here touches the DOM except `renderModelCards`, which builds
// elements and text nodes only, as everywhere else in `app/`.

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

/**
 * The provider cards. `server` is the value sent as `provider`; `fixed` means
 * the server picks the endpoint and ignores any `base_url`, so the page shows
 * it rather than asking for it.
 */
export const PROVIDERS = Object.freeze([
  {
    id: "openai",
    label: "OpenAI",
    server: "openai",
    fixed: true,
    baseUrl: "https://api.openai.com/v1",
    models: ["gpt-4.1", "gpt-4.1-mini", "o4-mini"],
    keyHint: "Starts with sk-",
  },
  {
    id: "anthropic",
    label: "Anthropic",
    server: "anthropic",
    fixed: true,
    baseUrl: "https://api.anthropic.com/v1",
    models: ["claude-sonnet-4-5", "claude-haiku-4-5", "claude-opus-4-1"],
    keyHint: "Starts with sk-ant-",
  },
  {
    id: "deepseek",
    label: "DeepSeek",
    server: "deepseek",
    fixed: true,
    baseUrl: "https://api.deepseek.com/v1",
    models: ["deepseek-chat", "deepseek-reasoner"],
    keyHint: "Starts with sk-",
  },
  {
    id: "google",
    label: "Google Gemini",
    server: "custom",
    fixed: false,
    baseUrl: "https://generativelanguage.googleapis.com/v1beta/openai",
    models: ["gemini-2.5-pro", "gemini-2.5-flash"],
    keyHint: "A Google AI Studio key",
  },
  {
    id: "groq",
    label: "Groq",
    server: "custom",
    fixed: false,
    baseUrl: "https://api.groq.com/openai/v1",
    models: ["llama-3.3-70b-versatile", "openai/gpt-oss-120b"],
    keyHint: "Starts with gsk_",
  },
  {
    id: "openrouter",
    label: "OpenRouter",
    server: "openrouter",
    fixed: true,
    baseUrl: "https://openrouter.ai/api/v1",
    models: ["anthropic/claude-sonnet-4.5", "openai/gpt-4.1", "deepseek/deepseek-chat"],
    keyHint: "Starts with sk-or-",
  },
  {
    id: "custom",
    label: "Custom",
    server: "custom",
    fixed: false,
    baseUrl: "",
    models: [],
    keyHint: "The key your endpoint expects",
  },
]);

/** One role by id, or undefined. */
export function roleInfo(id) {
  return ROLES.find((role) => role.id === id);
}

/** One provider card by id, or undefined. */
export function providerInfo(id) {
  return PROVIDERS.find((provider) => provider.id === id);
}

/**
 * The card a stored connection belongs to. A `custom` row is matched back to
 * Google or Groq by its base URL, so a Replace opens the card it came from.
 */
export function providerForConnection(row) {
  const server = row && row.provider;
  if (server && server !== "custom") {
    return providerInfo(server) || providerInfo("custom");
  }
  const base = String((row && row.base_url) || "").replace(/\/+$/, "");
  return (
    PROVIDERS.find((p) => !p.fixed && p.baseUrl && p.baseUrl === base) ||
    providerInfo("custom")
  );
}

/**
 * Checks the connect form. Answers `{ ok, errors, value }`: `errors` maps a
 * field (`role`, `provider`, `baseUrl`, `apiKey`, `model`) to the sentence
 * shown under it, and `value` is the connection `LB.setting("models", role,
 * value)` sends when `ok`.
 *
 * The URL rule is the server's: HTTPS only (its SSRF guard refuses anything
 * else, and a key must never travel in clear text).
 */
export function validateConnect(form) {
  const errors = {};
  const role = roleInfo(form && form.role);
  const provider = providerInfo(form && form.provider);
  if (!role) errors.role = "Choose what this model is for.";
  if (!provider) errors.provider = "Choose a provider.";

  let baseUrl = null;
  if (provider && !provider.fixed) {
    const raw = String((form && form.baseUrl) || "").trim();
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
        errors.baseUrl = "That is not a URL. It should look like https://api.example.com/v1.";
      } else if (parsed.protocol !== "https:") {
        errors.baseUrl = "Use an https:// URL. Keys are never sent in clear text.";
      } else if (parsed.username || parsed.password) {
        errors.baseUrl = "Put the key in the API key field, not in the URL.";
      } else {
        baseUrl = raw.replace(/\/+$/, "");
      }
    }
  }

  const apiKey = String((form && form.apiKey) || "").trim();
  if (!apiKey) errors.apiKey = "Paste the API key for this provider.";

  const model = String((form && form.model) || "").trim();
  if (!model) errors.model = "Enter the model name, for example one of the suggestions.";

  const ok = Object.keys(errors).length === 0;
  return {
    ok,
    errors,
    role: role ? role.id : null,
    value: ok
      ? {
          provider: provider.server,
          base_url: baseUrl,
          api_key: apiKey,
          model,
          fallback_to_managed: false,
        }
      : null,
  };
}

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
    providerLabel: provider.id === "custom" ? "Custom endpoint" : provider.label,
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
      pair.appendChild(el("dd", null, value));
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
