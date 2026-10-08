// The browser-only glue: DOM helpers, the fetch wrapper, the theme, and the
// one settings write path. Everything here is browser-side; the pure logic the
// tests need lives in `markdown.js` and `settings.js`, which the tests import
// directly, so the tests exercise the code the pages actually run.
//
// No request below ever carries an Authorization header, and none may: the
// session is an HttpOnly cookie the browser attaches on its own; every request
// asks the fetch wrapper to send credentials same-origin (or to include them
// when the API is cross-origin, which CORS allows only for the origins
// `livingbrain-venture` lists). The one token value the app ever holds — the
// one `POST /v1/tokens` returns — is written into a single element by
// `revealToken` for the member to copy, and never stored. See `tokens.js`.

import { settingRequest, describeOutcome } from "./settings.js";
import {
  tokenListRequest,
  tokenCreateRequest,
  tokenRevokeRequest,
  tokenViews,
  createdTokenView,
  tokenErrorMessage,
  renderTokenRows,
  revealToken,
  clearToken,
  safeReturnTo,
  RETURN_TO_PARAM,
} from "./tokens.js";
import { renderMarkdown, renderCitations, renderBacklinks } from "./markdown.js";

/**
 * The API base. Same-origin by default, because the app is served from the
 * same host as the Worker in the hosted deployment; a local preview can point
 * it elsewhere with `<meta name="lb-api" content="http://localhost:8787">`.
 * A white-label deployment sets the same meta tag.
 */
function apiBase() {
  const meta =
    typeof document === "undefined"
      ? null
      : document.querySelector('meta[name="lb-api"]');
  const value = meta ? meta.getAttribute("content") : "";
  return (value || "").replace(/\/+$/, "");
}

/** One element, by CSS selector or by id shorthand (`#id`). */
export const $ = (sel, root) => (root || document).querySelector(sel);

/** A list of elements as a real array. */
export const $$ = (sel, root) => Array.from((root || document).querySelectorAll(sel));

/**
 * Escapes text for an HTML attribute or text node.
 *
 * Nothing in this app builds HTML from user content with `innerHTML` — the
 * wiki renderer emits node descriptions and `mountNodes` creates elements —
 * so this helper exists for the rare case where a value must be put into a
 * `template` string (the pages' own static markup, never a page body).
 */
export const esc = (value) =>
  String(value == null ? "" : value)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");

/**
 * An API error carrying the server's problem-details body. The API answers
 * either `{error}` or `{message}` (the CLI's rule), so both are read; `detail`
 * is Cratefield's extra and is shown when present.
 */
export class ApiError extends Error {
  constructor(status, body, path) {
    const message =
      (body && (body.error || body.message || body.detail || body.title)) ||
      `request to ${path} failed`;
    super(typeof message === "string" ? message : JSON.stringify(message));
    this.name = "ApiError";
    this.status = status;
    this.body = body;
    this.path = path;
  }
}

/**
 * The one fetch wrapper.
 *
 * Sends the session cookie, parses JSON when the answer is JSON, and turns any
 * non-2xx into an `ApiError` carrying the problem-details body. It never
 * retries and never handles a token.
 */
export async function api(path, options = {}) {
  const { method = "GET", body, headers = {} } = options;
  const url = path.startsWith("http") ? path : `${apiBase()}${path}`;
  const init = {
    method,
    // `same-origin` sends the cookie for the deployed app; `include` is
    // needed only when the API is on another origin, which CORS allows.
    credentials:
      typeof location !== "undefined" &&
      url.startsWith("http") &&
      !url.startsWith(location.origin)
        ? "include"
        : "same-origin",
    headers: { Accept: "application/json", ...headers },
  };
  if (body !== undefined) {
    init.headers["Content-Type"] = "application/json";
    init.body = JSON.stringify(body);
  }
  let response;
  try {
    response = await fetch(url, init);
  } catch (cause) {
    throw new ApiError(0, { message: "the network request failed" }, path);
  }
  const text = await response.text();
  let parsed = null;
  if (text) {
    try {
      parsed = JSON.parse(text);
    } catch {
      parsed = { message: text.slice(0, 300) };
    }
  }
  if (!response.ok) throw new ApiError(response.status, parsed, path);
  return parsed;
}

/**
 * Builds DOM nodes from the descriptions `markdown.js` returns.
 *
 * The only way content reaches the document is `textContent` or
 * `setAttribute` — there is no `innerHTML`, no `insertAdjacentHTML`, and no
 * `document.write` anywhere in `app/`, so a wiki page body cannot become
 * executable markup.
 */
export function mountNodes(nodes, into) {
  const target = into || document.createDocumentFragment();
  // `renderMarkdown` answers with a list of blocks and `renderCitations` /
  // `renderBacklinks` with a single element, so a lone node is wrapped rather
  // than iterated as an object.
  const list = Array.isArray(nodes) ? nodes : nodes ? [nodes] : [];
  for (const node of list) {
    if (node.type === "text") {
      target.appendChild(document.createTextNode(node.value));
      continue;
    }
    const el = document.createElement(node.tag);
    for (const [name, value] of Object.entries(node.attrs || {})) {
      // `hidden` is set as a property rather than an attribute so the element
      // is genuinely not rendered.
      if (name === "hidden") el.hidden = true;
      else el.setAttribute(name, String(value));
    }
    mountNodes(node.children, el);
    target.appendChild(el);
  }
  return target;
}

/** Replaces a container's children with freshly mounted nodes. */
export function renderInto(container, nodes) {
  container.replaceChildren();
  container.appendChild(mountNodes(nodes));
}

/* ------------------------------------------------------------------ theme */

const THEME_KEY = "lb-theme";

/** Reads the stored choice: "light", "dark", or null for "follow the system". */
function storedTheme() {
  try {
    const value = localStorage.getItem(THEME_KEY);
    return value === "light" || value === "dark" ? value : null;
  } catch {
    return null;
  }
}

/**
 * Applies a theme. `null` means "follow the system", which is the default:
 * light when the system asks for light, dark otherwise.
 */
export function applyTheme(theme) {
  const root = document.documentElement;
  if (theme === "light" || theme === "dark") {
    root.dataset.theme = theme;
  } else {
    delete root.dataset.theme;
  }
  try {
    if (theme) localStorage.setItem(THEME_KEY, theme);
    else localStorage.removeItem(THEME_KEY);
  } catch {
    /* storage can be refused; the theme still applies for this page */
  }
  // Anything that mirrors the theme (a button label) listens for this.
  document.dispatchEvent(new CustomEvent("lb-theme", { detail: { theme } }));
  return theme;
}

/** The theme in force right now, with the system preference resolved. */
export function currentTheme() {
  const chosen = storedTheme();
  if (chosen) return chosen;
  return window.matchMedia("(prefers-color-scheme: light)").matches
    ? "light"
    : "dark";
}

/** Wires a toggle button: click cycles dark → light → system. */
export function wireThemeToggle(button) {
  if (!button) return;
  const paint = () => {
    const theme = currentTheme();
    button.textContent = theme === "dark" ? "Light theme" : "Dark theme";
    button.setAttribute(
      "aria-label",
      theme === "dark" ? "Switch to the light theme" : "Switch to the dark theme",
    );
  };
  button.addEventListener("click", () => {
    const theme = currentTheme();
    applyTheme(theme === "dark" ? "light" : "dark");
    paint();
  });
  document.addEventListener("lb-theme", paint);
  paint();
}

/* --------------------------------------------------------------- settings */

/**
 * THE settings write path.
 *
 * Every settings change in the app goes through this one function. The pages
 * never build a settings request themselves and never call `api()` for a
 * settings route; they call `LB.setting(section, key, value)`.
 *
 * Two things make the audit criterion (#30) satisfiable from here:
 *
 *   1. There is exactly one place a setting is mutated, so a reviewer can grep
 *      for `LB.setting(` and account for every write the UI can make.
 *   2. The `section`, `key` and `value` travel *with* the change — the body
 *      always carries them, alongside the model fields the API needs — so the
 *      server can attribute each write to a named setting without guessing
 *      from the URL or from a diff.
 *
 * The request itself is built by the pure `settingRequest` in `settings.js`,
 * and this function is exercised end to end (including the bytes that reach
 * the wire) by the tests in `app/tests/`.
 */
export async function setSetting(section, key, value) {
  const request = settingRequest(section, key, value);
  if (!request.wired) {
    // No server route yet: say so rather than sending a request that 404s.
    return { ok: false, sent: false, message: describeOutcome(request, false) };
  }
  try {
    await api(request.path, { method: request.method, body: request.body });
    return { ok: true, sent: true, message: describeOutcome(request, true) };
  } catch (error) {
    const detail = error instanceof ApiError ? error.message : String(error);
    return { ok: false, sent: true, message: detail };
  }
}

/* ------------------------------------------------------------------ pages */

/** Shows a message in a notice element. */
export function notice(element, message, tone = "warn") {
  if (!element) return;
  element.textContent = message || "";
  if (message) element.dataset.tone = tone;
  else delete element.dataset.tone;
}

const pages = {};

/** Runs a page's setup once the DOM is parsed. */
function boot() {
  wireThemeToggle($("#theme-toggle"));
  const init = document.body && document.body.dataset.page;
  const page = pages[init];
  if (page) page().catch((error) => {
    notice($("#notice"), String(error && error.message ? error.message : error), "bad");
  });
  if ("serviceWorker" in navigator && location.protocol === "https:") {
    navigator.serviceWorker.register("sw.js").catch(() => {
      /* an unavailable service worker is not an error worth showing */
    });
  }
}

/* --- sign-in -------------------------------------------------------- */

/**
 * Where a pending sign-in return target waits out the round trip.
 *
 * The device-approval hook bounces an anonymous visitor here as
 * `/index.html?return_to=<the approval page>`, but both ways in leave this
 * page — the magic link is opened from a mail client, Slack finishes on its
 * own callback — and both land back on this page at `/` with an empty query,
 * so the target cannot ride in the URL. The session itself is cookie-only and
 * `tokens.js` is checked by a static test that forbids storage there, so the
 * target — not a secret, and gone when the tab closes — waits in
 * `sessionStorage` in this file.
 */
const RETURN_TO_KEY = "lb-return-to";

/**
 * Remembers a fresh `return_to` and answers where to go now.
 *
 * Called once per sign-in page load with the page's query string and the
 * session store. A `return_to` in the query means the person has just been
 * bounced here by the approval hook: only a path `safeReturnTo` accepts is
 * stored, and it replaces any older one — it is the most recent bounce; an
 * unsafe value is dropped rather than stored. No `return_to` means the person
 * is back from a completed sign-in: the stored target is taken out,
 * re-validated, and returned for the caller to navigate to. Any storage
 * failure degrades to "no round trip to resume", never to a throw.
 */
export function resumeReturnTo(search, store) {
  const params = new URLSearchParams(search);
  if (params.has(RETURN_TO_PARAM)) {
    const requested = safeReturnTo(params.get(RETURN_TO_PARAM));
    try {
      if (requested) store.setItem(RETURN_TO_KEY, requested);
      else store.removeItem(RETURN_TO_KEY);
    } catch {
      /* storage can be refused; the person signs in without the bounce back */
    }
    return null;
  }
  let stored = null;
  try {
    stored = store.getItem(RETURN_TO_KEY);
    store.removeItem(RETURN_TO_KEY);
  } catch {
    return null;
  }
  return safeReturnTo(stored);
}

pages.signin = async function signin() {
  // A returned target means this load is the landing after a completed
  // sign-in: go straight back to where the person was headed — the
  // device-approval page, usually — instead of showing the form again.
  const resume = resumeReturnTo(location.search, sessionStorage);
  if (resume) {
    location.assign(resume);
    return;
  }
  const form = $("#email-form");
  const status = $("#email-status");
  const email = $("#email");

  // Magic link: always 202 {status:"accepted"}, whatever the address is, so
  // the copy below never claims an address exists.
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    notice(status, "Sending the link…", "warn");
    try {
      await api("/v1/workspaces/email/start", {
        method: "POST",
        body: { email: email.value.trim() },
      });
      notice(
        status,
        "If that address can receive mail, a sign-in link is on its way. " +
          "The same answer is given for every address, so nothing here says " +
          "whether this one is a member.",
        "ok",
      );
      form.reset();
    } catch (error) {
      notice(status, error.message, "bad");
    }
  });

  // Slack is one of four equal options, not the privileged one. The route
  // exists server-side; the button is a plain link so it degrades to nothing
  // rather than to a broken fetch.
  const slack = $("#signin-slack");
  slack.href = `${apiBase()}/v1/workspaces/slack/start`;
};

/* --- settings ------------------------------------------------------- */

pages.settings = async function settings() {
  const status = $("#settings-notice");
  const modelBody = $("#model-rows");
  const modelsCard = $("#models-status");

  const load = async () => {
    modelBody.replaceChildren();
    try {
      const rows = await api("/v1/models");
      const list = Array.isArray(rows) ? rows : [];
      for (const row of list) {
        const tr = document.createElement("tr");
        const cells = [
          row.role,
          row.provider,
          row.model,
          row.key, // `…abcd`: the server never returns the full key.
          row.status,
        ];
        for (const value of cells) {
          const td = document.createElement("td");
          td.textContent = value == null ? "—" : String(value);
          tr.appendChild(td);
        }
        modelBody.appendChild(tr);
      }
      if (!list.length) {
        const tr = document.createElement("tr");
        const td = document.createElement("td");
        td.colSpan = 5;
        td.className = "muted";
        td.textContent =
          "No model connections yet. The managed model is used until one is connected.";
        tr.appendChild(td);
        modelBody.appendChild(tr);
      }
      notice(modelsCard, "", "ok");
    } catch (error) {
      notice(
        modelsCard,
        `Could not read the model connections: ${error.message}`,
        "warn",
      );
    }
  };

  // Connect a model. The role is the key in the URL, so it is also the key in
  // the audit record; the value is the whole connection the server reads.
  $("#connect-model").addEventListener("click", async () => {
    const role = $("#role").value;
    const value = {
      provider: $("#provider").value,
      base_url: $("#base-url").value.trim() || null,
      api_key: $("#api-key").value,
      model: $("#model-name").value.trim(),
      fallback_to_managed: false,
    };
    const outcome = await setSetting("models", role, value);
    notice(status, outcome.message, outcome.ok ? "ok" : "warn");
    if (outcome.ok) {
      // The key is never echoed back by the API, so it is dropped here too.
      $("#api-key").value = "";
      await load();
    }
  });

  await load();
  await wireTokens();
};

/* --- api tokens ------------------------------------------------------ */

/**
 * The token section. Three rules, all about where a secret may live: the list
 * is rendered from `tokenViews`, which cannot carry a value; the value a create
 * returns is written once into `#token-value`, beside "not shown again"; and
 * dismissing it — or leaving the page — takes it back out.
 */
function wireTokens() {
  const status = $("#tokens-status");
  const rows = $("#token-rows");
  const reveal = $("#token-reveal");
  const value = $("#token-value");

  const forget = () => {
    clearToken(value);
    reveal.hidden = true;
  };

  const load = async () => {
    try {
      renderTokenRows(rows, tokenViews(await api(tokenListRequest().path)));
      notice(status, "");
    } catch (error) {
      renderTokenRows(rows, []);
      notice(status, tokenErrorMessage(error, "read your tokens"), "warn");
    }
  };

  $("#create-token").addEventListener("click", async () => {
    let request;
    try {
      request = tokenCreateRequest($("#token-name").value, $("#token-scopes").value);
    } catch (error) {
      return notice(status, error.message, "warn");
    }
    try {
      const created = await api(request.path, {
        method: request.method,
        body: request.body,
      });
      revealToken(value, createdTokenView(created));
      reveal.hidden = false;
      notice(status, "Token created. Copy it now — it is not shown again.", "ok");
      $("#token-name").value = "";
      $("#token-scopes").value = "";
      await load();
    } catch (error) {
      notice(status, tokenErrorMessage(error, "create the token"), "bad");
    }
  });

  // One handler for the table: each row's button carries its own prefix, so the
  // list can be re-rendered without re-attaching anything.
  rows.addEventListener("click", async (event) => {
    const button = event.target.closest("[data-prefix]");
    if (!button) return;
    const name = button.dataset.name;
    // A revoke is immediate and cannot be undone, so it is confirmed.
    if (!window.confirm(`Revoke "${name}"? Anything using it stops working at once.`)) return;
    try {
      const request = tokenRevokeRequest(button.dataset.prefix);
      await api(request.path, { method: request.method });
      notice(status, `Revoked ${name}.`, "ok");
      await load();
    } catch (error) {
      notice(status, tokenErrorMessage(error, "revoke the token"), "bad");
    }
  });

  $("#copy-token").addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(value.textContent);
      notice(status, "Token copied to the clipboard.", "ok");
    } catch {
      // Clipboard access can be refused; say where the value is rather than
      // pretending the copy worked.
      notice(status, "Select the token above and copy it by hand.", "warn");
    }
  });

  $("#dismiss-token").addEventListener("click", () => {
    forget();
    notice(status, "The token is gone from this page.", "ok");
  });

  // Leaving the page is a dismissal too.
  window.addEventListener("pagehide", forget);

  return load();
}

/* --- wiki ----------------------------------------------------------- */

pages.wiki = async function wiki() {
  const params = new URLSearchParams(location.search);
  const slug = params.get("slug") || "";
  const title = $("#page-title");
  const body = $("#page-body");
  const backlinks = $("#page-backlinks");
  const citations = $("#page-citations");
  // Two places to say something: a short line in the toolbar, and a notice
  // below it for anything long enough to need explaining.
  const status = $("#wiki-status");
  const banner = $("#wiki-notice");
  const say = (message, tone) => {
    status.textContent = message;
    notice(banner, message, tone);
  };
  const editorWrap = $("#editor-wrap");
  const viewWrap = $("#view-wrap");
  const source = $("#source");
  const preview = $("#preview");
  const toggle = $("#mode-toggle");

  let current = null;
  let version = null;

  const show = (page) => {
    current = page;
    version = page.version == null ? null : page.version;
    title.textContent = page.title || page.slug || "Untitled";
    renderInto(body, renderMarkdown(page.markdown || ""));
    renderInto(backlinks, renderBacklinks(page.backlinks || []));
    renderInto(citations, renderCitations(page.citations || []));
    if (source.value !== (page.markdown || "")) source.value = page.markdown || "";
    renderInto(preview, renderMarkdown(source.value));
  };

  const load = async () => {
    if (!slug) {
      say("Open a page with ?slug=<page-slug>.", "warn");
      return;
    }
    try {
      show(await api(`/v1/pages/${encodeURIComponent(slug)}`));
      say("", "ok");
    } catch (error) {
      say(`Could not open ${slug}: ${error.message}`, "bad");
    }
  };

  // Live preview: the same renderer the view mode uses, so what is previewed
  // is what will be saved.
  source.addEventListener("input", () => {
    renderInto(preview, renderMarkdown(source.value));
  });

  const setMode = (mode) => {
    const editing = mode === "edit";
    editorWrap.hidden = !editing;
    viewWrap.hidden = editing;
    // Saving only makes sense while editing, so the button lives in edit mode.
    save.hidden = !editing;
    toggle.textContent = editing ? "Done editing" : "Edit";
    toggle.setAttribute("aria-pressed", editing ? "true" : "false");
  };
  toggle.addEventListener("click", () => {
    setMode(editorWrap.hidden ? "edit" : "view");
  });
  setMode("view");

  const save = $("#save");
  save.addEventListener("click", async () => {
    if (!current) return;
    // Optimistic concurrency: the store refuses a write whose base_version is
    // stale, so a lost update is a visible conflict, never a silent overwrite.
    const body = {
      markdown: source.value,
      base_version: version,
      title: current.title,
    };
    try {
      const saved = await api(`/v1/pages/${encodeURIComponent(current.slug || slug)}`, {
        method: "PUT",
        body,
      });
      show(saved && saved.markdown != null ? saved : { ...current, ...body, version: (version || 0) + 1 });
      setMode("view");
      say("Saved.", "ok");
    } catch (error) {
      if (error instanceof ApiError && error.status === 409) {
        say(
          "Someone else saved this page while you were editing. Your text is " +
            "still here — reload to get their version, then re-apply your " +
            "changes.",
          "bad",
        );
        return;
      }
      say(`Not saved: ${error.message}`, "bad");
    }
  });

  await load();
};

/**
 * `LB` is the app's whole public surface: the DOM helpers, the fetch wrapper,
 * the theme, and `LB.setting`, which is the settings choke point — the *only*
 * way a setting is changed anywhere in `app/`, which is what makes the audit
 * criterion (#30) checkable by grepping for `LB.setting(`.
 *
 * It hangs off `window` in a browser and off `globalThis` under Node, which is
 * how `node --test app/tests/` can import this file and exercise `api` and
 * `LB.setting` themselves rather than a copy of them. Booting is guarded so an
 * import in a test does not try to touch the DOM.
 */
export const LB = Object.freeze({
  $, $$, esc,
  api, ApiError,
  setting: setSetting,
  theme: { apply: applyTheme, current: currentTheme },
});

globalThis.LB = LB;

if (typeof document !== "undefined") {
  window.LB = LB;
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", boot);
  } else {
    boot();
  }
}