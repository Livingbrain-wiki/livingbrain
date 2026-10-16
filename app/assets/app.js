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
  PROVIDERS,
  CATALOG_SOURCE,
  WIRES,
  AUTHS,
  roleInfo,
  providerInfo,
  providerForConnection,
  searchProviders,
  fillTemplate,
  keyHint,
  validateConnect,
  discoverRequest,
  slotView,
  slotViews,
  renderModelSlots,
  ROLES,
  BADGES,
  cardInfo,
  cardForConnection,
  runnerCardFor,
  protocolLabel,
  modelOptions,
  modelOptionLabel,
  cardViews,
  renderModelCards,
  mainModelChoices,
  lastSeenText,
  runnerValue,
  runnerCommand,
  providerHost,
  maskedKey,
  statusView,
  jevView,
  jevChange,
  jevTestBody,
  jevTestMessage,
  JEV_PRIVACY,
} from "./models.js";
import { providerMark } from "./marks.js";
import {
  TOKEN_SCOPES,
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
import {
  membersRequest,
  invitesRequest,
  workspacesRequest,
  inviteRequest,
  roleChangeRequest,
  removeRequest,
  withdrawRequest,
  switchRequest,
  createWorkspaceRequest,
  deleteWorkspaceRequest,
  deleteConfirmMatches,
  deleteZone,
  deleteErrorMessage,
  memberViews,
  inviteViews,
  canManage,
  assignableRoles,
  memberErrorMessage,
  renderMemberRows,
  renderInviteRows,
  renderWorkspaces,
} from "./members.js";
import {
  budgetsRequest,
  budgetView,
  budgetPatch,
  budgetErrorMessage,
  renderBudgets,
  readInputs,
  readSavers,
  matchPreset,
  presetInputs,
  FieldError,
} from "./budgets.js";
import {
  agentListRequest,
  agentCreateRequest,
  agentUpdateRequest,
  agentDeleteRequest,
  agentKeysRequest,
  agentKeyMintRequest,
  agentKeyRotateRequest,
  agentKeyRevokeRequest,
  grantListRequest,
  grantDeleteRequest,
  grantPlan,
  parseFolder,
  grantPreview,
  grantBatchRequest,
  grantRevokeBatchRequest,
  agentBatchRequest,
  needsConfirm,
  principalOptions,
  folderOptions,
  grantFilterOptions,
  filterGrants,
  keepSelected,
  selectionText,
  MAX_BATCH,
  auditRequest,
  agentViews,
  keyViews,
  mintedKey,
  grantViews,
  principalLabel,
  accessErrorMessage,
  renderAgentRows,
  renderKeyRows,
  renderGrantRows,
  renderAuditRows,
  revealAgentKey,
  clearAgentKey,
} from "./access.js";
import { isSafeHref } from "./markdown.js";
import {
  checkSession,
  applySignInPage,
  signInOffersSlack,
  applyHeader,
  signOutRequest,
  SLOTS,
  MCP_PATH,
} from "./session.js";
import { wikiPage, projectSegment } from "./wiki.js";
import { createMultiSelect } from "./multiselect.js";
import { mountShell } from "./shell.js";
import { brainPage } from "./brain.js";
import { dreamingPage } from "./dreaming.js";
import { settingsTab, SETTINGS_TABS } from "./nav.js";
import {
  nameError,
  displayNameRequest,
  workspaceNameRequest,
  avatarUploadRequest,
  avatarRemoveRequest,
  avatarFileError,
  profileView,
  profileErrorMessage,
  paintAvatar,
  AVATAR_SIDE,
  cropRect,
  panCrop,
  drawCrop,
  encodeAvatar,
  readImage,
} from "./profile.js";
import { mountGithub } from "./github.js";
import { mountDiscord } from "./discord.js";
import { startUpdateWatch } from "./update.js";

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
  const { method = "GET", body, raw, contentType, headers = {} } = options;
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
  if (raw !== undefined) {
    // A body that is not JSON — an avatar image — goes as it is, with its own
    // type; the browser sets its length.
    init.headers["Content-Type"] = contentType || (raw && raw.type) || "application/octet-stream";
    init.body = raw;
  } else if (body !== undefined) {
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
  try {
    const data = await api(request.path, { method: request.method, body: request.body });
    return { ok: true, sent: true, message: describeOutcome(request, true), data };
  } catch (error) {
    const detail = error instanceof ApiError ? error.message : String(error);
    const status = error instanceof ApiError ? error.status : 0;
    return { ok: false, sent: true, message: detail, status };
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

/* --- session -------------------------------------------------------- */

/**
 * The origin the API answers on: the `lb-api` meta when a deployment sets
 * one, this page's own origin otherwise. It is what the MCP endpoint and the
 * CLI's `LIVINGBRAIN_API_URL` are shown under, so staging shows staging.
 */
export function apiOrigin() {
  return apiBase() || (typeof location !== "undefined" ? location.origin : "");
}

/**
 * Asks the API to drop the session cookie. Resolves `true` once it has; on a
 * refusal the caller says so and leaves the page as it was.
 */
export async function signOut() {
  const request = signOutRequest();
  await api(request.path, { method: request.method });
  return true;
}

/**
 * The header on every page but sign-in: "Signed in as …" and Sign out when
 * there is a session, the "Sign in" link otherwise. It never blocks the
 * page: the check runs beside the page's own setup.
 */
async function wireHeaderSession() {
  const bar = $("#session-bar");
  if (!bar) return;
  const els = { bar, who: $("#session-who"), signin: $("#header-signin") };
  if (shell) shell.applySession({ kind: "checking" });
  const state = await checkSession(api);
  applyHeader(els, state);
  if (shell) shell.applySession(state);
  const button = $("#header-signout");
  if (button) {
    button.addEventListener("click", async () => {
      button.disabled = true;
      try {
        await signOut();
        location.assign("index.html");
      } catch (error) {
        button.disabled = false;
        notice($("#settings-notice") || $("#wiki-notice"), `Not signed out: ${error.message}`, "bad");
      }
    });
  }
}

/**
 * The app shell (`shell.js`): the sidebar, the drawer, the switcher, the
 * user menu and the palette. `null` where the page has no shell to mount
 * (a document without `getElementById` — the tests' stub DOM).
 */
let shell = null;

function mountAppShell() {
  try {
    shell = mountShell({
      api,
      origin: apiOrigin(),
      toggleTheme: () => applyTheme(currentTheme() === "dark" ? "light" : "dark"),
      signOut: async () => {
        try {
          await signOut();
        } catch {
          /* the cookie may already be gone; either way, back to the front door */
        }
        location.assign("index.html");
      },
    });
    // `wiki.js` switches views in place and says so with this event.
    document.addEventListener("lb-view", (event) => {
      if (shell && event && event.detail) shell.setActive(event.detail.view);
    });
  } catch {
    shell = null;
  }
}

/** Runs a page's setup once the DOM is parsed. */
function boot() {
  if (typeof document.getElementById === "function") mountAppShell();
  wireThemeToggle($("#theme-toggle"));
  wireHeaderSession().catch(() => {
    /* the header keeps its plain "Sign in" link */
  });
  const init = document.body && document.body.dataset.page;
  const page = pages[init];
  if (page) page().catch((error) => {
    notice($("#notice"), String(error && error.message ? error.message : error), "bad");
  });
  if ("serviceWorker" in navigator && location.protocol === "https:") {
    navigator.serviceWorker.register("sw.js").catch(() => {
      /* an unavailable service worker is not an error worth showing */
    });
    // A deploy installs a new worker, which takes over this tab at once; if
    // the modules running here are not that build's, offer a reload.
    startUpdateWatch(navigator.serviceWorker, document, import.meta.url, () => location.reload());
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
  // Signed in already? The magic link and Slack both land here at `/`, so
  // this is the page a fresh session arrives on. Neither the form nor the
  // home shows until the check settles.
  const els = {
    checking: $("#session-checking"),
    signin: $("#signin-view"),
    home: $("#home-view"),
    error: $("#session-error"),
  };
  for (const key of SLOTS) els[key] = $$(`[data-session="${key}"]`);
  applySignInPage(els, { kind: "checking" }, apiOrigin());
  const state = await checkSession(api);
  applySignInPage(els, state, apiOrigin());

  const signout = $("#signout");
  signout.addEventListener("click", async () => {
    signout.disabled = true;
    try {
      await signOut();
      applySignInPage(els, { kind: "signed-out" }, apiOrigin());
      $("#email").focus();
    } catch (error) {
      notice(els.error, `Not signed out: ${error.message}`, "bad");
      els.error.hidden = false;
    } finally {
      signout.disabled = false;
    }
  });

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

  // The Slack button is a plain link to the server's start route. It and its
  // divider stay hidden until the server says Slack is configured, so a failed
  // request leaves the email form on its own.
  const slack = $("#signin-slack");
  const divider = $("#signin-or");
  slack.href = `${apiBase()}/v1/workspaces/slack/start`;
  api("/v1/workspaces/sign-in-methods")
    .then((methods) => {
      const offered = signInOffersSlack(methods);
      slack.hidden = !offered;
      divider.hidden = !offered;
    })
    .catch(() => {});
};

/* --- the brain map ---------------------------------------------------- */

pages.brain = function brain() {
  return brainPage({ api, ApiError, notice, renderInto, checkSession });
};

/* --- settings ------------------------------------------------------- */

/**
 * The settings page shows one tab at a time (docs/ux-navigation.md): the
 * hash names the tab, the old anchors (`#connect`, `#soon`, `#stack`) land
 * on the tab that holds them, and the page head names the tab and its group.
 */
const TAB_HEADS = Object.freeze({
  models: ["Settings", "Models", "Bring your own model for each job the brain does. Keys are encrypted before they are stored."],
  limits: ["Settings", "Usage & limits", "How much the brain may spend on the workspace's model, and what it has spent."],
  integrations: ["Settings", "Integrations", "Connect your coding agent, GitHub, the CLI and the tools that read from the brain."],
  tokens: ["Settings", "API tokens", "Tokens let a coding agent or the CLI read your brain without a browser."],
  members: ["Workspace", "Members", "The workspace's name and picture, who is in it, and what they may do."],
  agents: ["Workspace", "Agent accounts", "Non-human accounts for your agents and services: each signs in only with its own API keys, and reads nothing until you grant it."],
  access: ["Workspace", "Access & audit", "Who may read and write which part of the brain, and the log of who did what."],
  account: ["Settings", "Account", "Your name and picture, your workspaces, and signing out."],
});

export function showSettingsTab(hash) {
  const tab = settingsTab(hash);
  const shown = new Set(SETTINGS_TABS[tab]);
  for (const sections of Object.values(SETTINGS_TABS)) {
    for (const id of sections) {
      const section = document.getElementById(id);
      if (!section) continue;
      section.hidden =
        !shown.has(id) || (section.hasAttribute("data-admin-only") && section.dataset.allowed !== "true");
      // A tab of one section: its own heading repeats the page's, so it is
      // kept for screen readers (the section's label) and hidden from view.
      section.classList.toggle("sect--solo", shown.size === 1);
    }
  }
  const [eyebrow, title, lede] = TAB_HEADS[tab];
  const head = { eyebrow: $("#settings-eyebrow"), title: $("#settings-title"), lede: $("#settings-lede") };
  if (head.eyebrow) head.eyebrow.textContent = eyebrow;
  if (head.title) head.title.textContent = title;
  if (head.lede) head.lede.textContent = lede;
  document.title = `${title} — Living Brain`;
  if (shell) shell.setActive();
  return tab;
}

function wireSettingsTabs() {
  showSettingsTab(location.hash);
  window.addEventListener("hashchange", () => {
    showSettingsTab(location.hash);
    window.scrollTo(0, 0);
  });
  const signout = $("#account-signout");
  if (signout) {
    signout.addEventListener("click", async () => {
      signout.disabled = true;
      try {
        await signOut();
        location.assign("index.html");
      } catch (error) {
        signout.disabled = false;
        notice($("#settings-notice"), `Not signed out: ${error.message}`, "bad");
      }
    });
  }
}

pages.settings = async function settings() {
  wireSettingsTabs();
  wireSectionNav();
  wireCopyButtons();
  fillConnectPanel();
  await Promise.all([
    wireModels(),
    wireTokens(),
    wireMembers(),
    wireDeleteWorkspace(),
    wireAccess(),
    wireProfile(),
    wireBudgets(),
    mountGithub({
      api,
      confirm: confirmAction,
      notify: notice,
      document,
      location,
      history: window.history,
    }).catch((error) => notice($("#github-status"), `Could not load GitHub: ${error.message}`, "bad")),
    mountDiscord({
      api,
      confirm: confirmAction,
      notify: notice,
      document,
      location,
      history: window.history,
    }).catch(() => null),
  ]);
};

/* --- small shared pieces ------------------------------------------- */

/** A short-lived message at the bottom of the screen. */
let toastTimer = null;
export function toast(message, tone = "ok") {
  const box = $("#toast");
  if (!box) return;
  box.textContent = message;
  box.dataset.tone = tone;
  box.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    box.hidden = true;
  }, tone === "bad" ? 8000 : 4000);
}

/**
 * Asks before something that cannot be undone. Resolves `true` only when the
 * person picks the confirm button; Escape and Cancel are `false`. Falls back
 * to `window.confirm` where `<dialog>` is missing.
 */
export function confirmAction({ title, body, confirm }) {
  const dialog = $("#confirm");
  if (!dialog || typeof dialog.showModal !== "function") {
    return Promise.resolve(window.confirm(`${title}\n\n${body}`));
  }
  $("#confirm-title").textContent = title;
  $("#confirm-body").textContent = body;
  $("#confirm-ok").textContent = confirm;
  dialog.returnValue = "";
  dialog.showModal();
  return new Promise((resolve) => {
    dialog.addEventListener("close", () => resolve(dialog.returnValue === "ok"), {
      once: true,
    });
  });
}

/** Every `[data-copy="<id>"]` button copies that element's text. */
function wireCopyButtons() {
  document.addEventListener("click", async (event) => {
    const button = event.target.closest("[data-copy]");
    if (!button) return;
    const source = document.getElementById(button.dataset.copy);
    if (!source) return;
    try {
      await navigator.clipboard.writeText(source.textContent);
      const label = button.textContent;
      button.textContent = "Copied";
      setTimeout(() => {
        button.textContent = label;
      }, 1500);
    } catch {
      toast("Copying was blocked. Select the text and copy it by hand.", "warn");
    }
  });
}

/** Marks the section in view in the section nav. */
function wireSectionNav() {
  const links = $$(".snav a");
  if (!links.length || typeof IntersectionObserver === "undefined") return;
  const mark = (id) => {
    for (const link of links) {
      if (link.getAttribute("href") === `#${id}`) link.setAttribute("aria-current", "true");
      else link.removeAttribute("aria-current");
    }
  };
  const observer = new IntersectionObserver(
    (entries) => {
      const visible = entries.filter((e) => e.isIntersecting);
      if (visible.length) mark(visible[0].target.id);
    },
    { rootMargin: "-30% 0px -60% 0px" },
  );
  for (const link of links) {
    const target = document.querySelector(link.getAttribute("href"));
    if (target) observer.observe(target);
  }
}

/** Builds a group of radio "cards" from a list of options. */
function choiceCards(container, name, options, selected, withMarks) {
  container.replaceChildren();
  for (const option of options) {
    const label = document.createElement("label");
    label.className = "choice__card";
    const input = document.createElement("input");
    input.type = "radio";
    input.name = name;
    input.value = option.value;
    if (option.value === selected) input.checked = true;
    label.appendChild(input);
    if (withMarks) label.appendChild(providerMark(option.value, option.label));
    const text = document.createElement("span");
    text.className = "choice__text";
    const title = document.createElement("strong");
    title.textContent = option.label;
    text.appendChild(title);
    if (option.detail) {
      const detail = document.createElement("span");
      detail.textContent = option.detail;
      text.appendChild(detail);
    }
    label.appendChild(text);
    container.appendChild(label);
  }
}

/** The checked value of a radio group, or "" when none is. */
const checked = (form, name) => {
  const input = form.querySelector(`input[name="${name}"]:checked`);
  return input ? input.value : "";
};

/** Shows (or clears) one field's error under it. */
function fieldError(id, message, input) {
  const slot = $(`#err-${id}`);
  if (slot) slot.textContent = message || "";
  if (input) {
    if (message) input.setAttribute("aria-invalid", "true");
    else input.removeAttribute("aria-invalid");
  }
}

/* --- connect your coding agent -------------------------------------- */

/** Writes this origin's endpoint and commands into the Connect panel. */
function fillConnectPanel() {
  const origin = apiOrigin();
  const mcp = `${origin}${MCP_PATH}`;
  const set = (id, text) => {
    const el = $(`#${id}`);
    if (el) el.textContent = text;
  };
  set("mcp-url", mcp);
  set(
    "cmd-claude",
    `claude mcp add --transport http livingbrain ${mcp} --header "Authorization: Bearer <token>"`,
  );
  set("cmd-cli", `LIVINGBRAIN_API_URL=${origin} livingbrain login`);
}

/* --- models ---------------------------------------------------------- */

/**
 * Settings → Models: the "Main model" picker, the provider cards, the job
 * slots, and one detail sheet (a `<dialog>`) per card. An API card's sheet is
 * the connect form; a subscription card's sheet sets the member's own device
 * runner — their Claude Code or Codex, on their own machine, with their own
 * subscription. No subscription credential ever reaches this page or the
 * server: the runner is a choice, and the CLI on the device does the call.
 */
async function wireModels() {
  const sheet = $("#model-sheet");
  const form = $("#model-form");
  const cardsBox = $("#model-cards");
  const slots = $("#model-rows");
  const modelsStatus = $("#models-status");
  const mainPick = $("#main-model");
  const mainLine = $("#main-model-line");
  const baseUrl = $("#base-url");
  const editEndpoint = $("#edit-endpoint");
  const apiKey = $("#api-key");
  const model = $("#model-name");
  const modelSelect = $("#model-select");
  const modelNote = $("#model-note");
  const roleSelect = $("#model-role");
  const submit = $("#connect-model");
  const discover = $("#discover-models");
  const search = $("#provider-search");
  const list = $("#provider-list");
  const codingPlan = $("#coding-plan");
  const runnerBox = $("#sheet-runner");
  const runnerRole = $("#runner-role");
  const runnerModel = $("#runner-model");
  const runnerCustom = $("#runner-model-custom");
  const runnerSave = $("#runner-save");
  const jevBox = $("#sheet-jev");
  const jevEnabled = $("#jev-enabled");
  const jevKey = $("#jev-key");
  const jevModel = $("#jev-model");
  const jevSave = $("#jev-save");
  const jevTest = $("#jev-test");
  const jevRemoveKey = $("#jev-remove-key");
  const OTHER = "__other";

  let rows = [];
  let mine = [];
  // Jev's state (`GET /v1/models/jev`), or null before it loads or on a
  // server without the route.
  let jev = null;
  // Whether this server has the device-runner routes yet (`/v1/models/mine`).
  let runnersAvailable = true;
  let lastSeen = null;
  let editing = false;
  // What the server last listed, by provider: `{provider, models, listed, note}`.
  let listed = { provider: null, models: [] };
  let selectedId = "anthropic";
  let activeCard = cardInfo("anthropic");
  let choices = { current: "", options: [] };

  // The role pickers in the sheet: Main, Triage, Research.
  for (const select of [roleSelect, runnerRole]) {
    select.replaceChildren();
    for (const role of ROLES) {
      const option = document.createElement("option");
      option.value = role.id;
      option.textContent = `${role.label} — ${role.hint}`;
      select.appendChild(option);
    }
  }

  // The provider picker only shows on the Custom card: Custom endpoint, or any
  // of the catalog's providers by search.
  choiceCards(
    $("#provider-popular"),
    "provider",
    [{ value: "custom", label: "Custom endpoint", detail: "Any URL you name" }],
    "custom",
    true,
  );
  choiceCards($("#wire-choices"), "wire", WIRES, "openai");
  choiceCards($("#auth-choices"), "auth", AUTHS, "bearer");
  $("#provider-count").textContent = `${PROVIDERS.length}`;
  const source = $("#provider-source");
  if (CATALOG_SOURCE && CATALOG_SOURCE.commit) {
    const href = `https://github.com/${CATALOG_SOURCE.repo}/blob/${CATALOG_SOURCE.commit}/${CATALOG_SOURCE.path}`;
    if (isSafeHref(href)) source.setAttribute("href", href);
  }
  const renderList = () => {
    const matches = searchProviders(search.value);
    choiceCards(
      list,
      "provider-pick",
      matches.map((provider) => ({ value: provider.id, label: provider.name, detail: providerHost(provider) })),
      selectedId,
      true,
    );
    $("#provider-empty").hidden = matches.length > 0;
  };
  search.addEventListener("input", renderList);

  const select = (id) => {
    selectedId = id;
    for (const input of form.querySelectorAll('input[name="provider"], input[name="provider-pick"]')) {
      input.checked = input.value === id;
    }
  };
  const currentProvider = () => providerInfo(selectedId) || providerInfo("anthropic");
  const variableValues = () => {
    const values = {};
    for (const input of $$("#provider-vars input")) values[input.dataset.var] = input.value;
    return values;
  };

  const paintEndpoint = () => {
    const provider = currentProvider();
    const custom = provider.id === "custom";
    const unlocked = custom || editing;
    if (!unlocked) baseUrl.value = fillTemplate(provider.base_url, variableValues());
    baseUrl.readOnly = !unlocked;
    editEndpoint.hidden = custom;
    editEndpoint.textContent = editing ? "Reset" : "Edit";
    editEndpoint.setAttribute("aria-pressed", String(editing));
    $("#base-url-hint").textContent = custom
      ? "The API root, for example https://api.example.com. Paths are added for you."
      : editing
        ? "Your own host for this provider, such as a regional endpoint."
        : "From the provider catalog.";
  };

  /** Fills a model `<select>` with options and an "Other model id" entry. */
  const fillModelSelect = (selectEl, models, chosen) => {
    selectEl.replaceChildren();
    for (const entry of models) {
      const option = document.createElement("option");
      option.value = entry.id;
      option.textContent = modelOptionLabel(entry);
      selectEl.appendChild(option);
    }
    const other = document.createElement("option");
    other.value = OTHER;
    other.textContent = "Other model id…";
    selectEl.appendChild(other);
    const known = models.some((entry) => entry.id === chosen);
    selectEl.value = known ? chosen : models.length && !chosen ? models[0].id : OTHER;
    return known || (models.length > 0 && !chosen);
  };

  /** The API form's model picker: listed, else curated, else type it. */
  const paintModels = (chosen) => {
    const provider = currentProvider();
    const card = activeCard && activeCard.kind === "api" && !codingPlan.checked ? activeCard : null;
    const models = modelOptions(card, listed, provider.id);
    const keep = chosen !== undefined ? chosen : model.value;
    const picked = fillModelSelect(modelSelect, models, keep);
    model.hidden = picked && modelSelect.value !== OTHER;
    model.value = modelSelect.value === OTHER ? keep || "" : modelSelect.value;
    const note = listed.provider === provider.id ? listed.note : "";
    modelNote.textContent = note || "";
    modelNote.hidden = !note;
  };
  modelSelect.addEventListener("change", () => {
    const other = modelSelect.value === OTHER;
    model.hidden = !other;
    model.value = other ? "" : modelSelect.value;
    fieldError("model", "", model);
    if (other) model.focus();
  });

  const applyProvider = (id, { keepModel = false } = {}) => {
    const provider = providerInfo(id) || providerInfo("anthropic");
    select(provider.id);
    editing = false;
    const custom = provider.id === "custom";
    $("#custom-fields").hidden = !custom;
    if (custom) baseUrl.value = "";
    const vars = $("#provider-vars");
    vars.replaceChildren();
    for (const variable of provider.variables || []) {
      const field = document.createElement("div");
      field.className = "field";
      const label = document.createElement("label");
      label.htmlFor = `var-${variable.name}`;
      label.textContent = variable.label;
      const input = document.createElement("input");
      input.id = `var-${variable.name}`;
      input.type = "text";
      input.autocomplete = "off";
      input.spellcheck = false;
      input.placeholder = variable.placeholder || "";
      input.dataset.var = variable.name;
      if (variable.default) input.value = variable.default;
      input.setAttribute("aria-describedby", `err-var-${variable.name}`);
      input.addEventListener("input", () => {
        fieldError(`var-${variable.name}`, "", input);
        paintEndpoint();
      });
      const error = document.createElement("p");
      error.className = "field-error";
      error.id = `err-var-${variable.name}`;
      field.append(label, input, error);
      vars.appendChild(field);
    }
    const picked = $("#provider-picked");
    picked.replaceChildren();
    if (!custom) {
      picked.append("Provider: ");
      const strong = document.createElement("strong");
      strong.textContent = provider.name;
      picked.appendChild(strong);
      if (provider.site && isSafeHref(provider.site)) {
        picked.append(" · ");
        const site = document.createElement("a");
        site.href = provider.site;
        site.target = "_blank";
        site.rel = "noopener noreferrer";
        site.textContent = "get a key";
        picked.appendChild(site);
      }
    }
    $("#api-key-kind").textContent = custom ? "" : keyHint(provider);
    if (listed.provider !== provider.id) listed = { provider: null, models: [] };
    fieldError("baseUrl", "", baseUrl);
    fieldError("provider", "");
    paintEndpoint();
    paintModels(keepModel ? undefined : "");
  };

  form.addEventListener("change", (event) => {
    if (event.target.name === "provider" || event.target.name === "provider-pick") {
      applyProvider(event.target.value);
    }
    if (event.target.name === "wire") {
      fieldError("wire", "");
      $("#sheet-protocol").textContent = protocolLabel(null, event.target.value);
    }
    if (event.target.name === "auth") fieldError("auth", "");
  });
  editEndpoint.addEventListener("click", () => {
    editing = !editing;
    paintEndpoint();
    if (editing) baseUrl.focus();
  });
  $("#toggle-key").addEventListener("click", (event) => {
    const show = apiKey.type === "password";
    apiKey.type = show ? "text" : "password";
    event.currentTarget.textContent = show ? "Hide" : "Show";
    event.currentTarget.setAttribute("aria-pressed", String(show));
  });
  for (const [input, id] of [
    [baseUrl, "baseUrl"],
    [apiKey, "apiKey"],
    [model, "model"],
  ]) {
    input.addEventListener("input", () => fieldError(id, "", input));
  }

  // Switching the job in an API sheet shows what that job uses on this card.
  roleSelect.addEventListener("change", () => {
    if (!activeCard || activeCard.kind === "runner") return;
    const row = rows.find((r) => r.role === roleSelect.value && cardForConnection(r) === activeCard);
    paintModels(row ? row.model || "" : "");
    submit.textContent = row ? "Save and test" : "Connect";
  });

  const readForm = () => ({
    role: roleSelect.value,
    provider: selectedId,
    variables: variableValues(),
    baseUrl: baseUrl.value,
    editUrl: editing,
    wire: checked(form, "wire"),
    auth: checked(form, "auth"),
    apiKey: apiKey.value,
    model: modelSelect.value === OTHER ? model.value : modelSelect.value,
  });

  const showErrors = (errors) => {
    fieldError("provider", errors.provider);
    fieldError("baseUrl", errors.baseUrl, baseUrl);
    fieldError("wire", errors.wire);
    fieldError("auth", errors.auth);
    fieldError("apiKey", errors.apiKey, apiKey);
    fieldError("model", errors.model, model);
    for (const input of $$("#provider-vars input")) {
      fieldError(`var-${input.dataset.var}`, errors[`var:${input.dataset.var}`], input);
    }
    return form.querySelector('[aria-invalid="true"]');
  };

  // Listing models: the key goes to Living Brain's own API, which asks the
  // provider; the page never calls a provider. A provider with no list of
  // its own answers the curated one with a note — never a bare status.
  const listModels = async ({ quiet = false } = {}) => {
    const provider = currentProvider();
    const check = validateConnect({ ...readForm(), model: "listing" });
    const blocking = Object.keys(check.errors).filter((k) => k !== "role");
    if (blocking.length) {
      if (!quiet) showErrors(check.errors);
      return;
    }
    const request = discoverRequest(check.value);
    discover.disabled = true;
    const label = discover.textContent;
    discover.textContent = "Listing…";
    try {
      const answer = await api(request.path, { method: request.method, body: request.body });
      listed = {
        provider: provider.id,
        models: (answer && answer.models) || [],
        note: answer && answer.listed === false ? answer.note || DISCOVER_FALLBACK_NOTE : "",
      };
    } catch (error) {
      if (error instanceof ApiError && error.status === 422 && /key-refused/.test(JSON.stringify(error.body || {}))) {
        fieldError("apiKey", error.message, apiKey);
        if (!quiet) apiKey.focus();
      } else if (error instanceof ApiError && error.status === 403) {
        if (!quiet) toast("Only workspace owners and admins can list or connect models.", "warn");
      }
      // Anything else: the known models, said plainly.
      listed = { provider: provider.id, models: [], note: DISCOVER_UNREACHABLE_NOTE };
    } finally {
      discover.disabled = false;
      discover.textContent = label;
      paintModels();
    }
  };
  discover.addEventListener("click", () => listModels());
  apiKey.addEventListener("change", () => listModels({ quiet: true }));

  /* ---- the Z.AI Coding Plan switch ---- */
  const paintCodingPlan = () => {
    const plan = activeCard && activeCard.coding_plan;
    $("#coding-plan-field").hidden = !plan;
    const on = Boolean(plan && codingPlan.checked);
    $("#coding-plan-label").textContent = plan ? plan.label : "";
    $("#connect-title").textContent = on ? "GLM Coding Plan" : "Connect with an API key";
    const note = $("#coding-plan-note");
    note.hidden = !on;
    note.replaceChildren();
    if (on) {
      note.append(`${plan.note} `);
      if (plan.guide && isSafeHref(plan.guide)) {
        const guide = document.createElement("a");
        guide.href = plan.guide;
        guide.target = "_blank";
        guide.rel = "noopener noreferrer";
        guide.textContent = "Z.AI's Claude Code guide";
        note.appendChild(guide);
      }
    }
    // With the plan on, the key form is not the way: the device runner is.
    // CSS hides the rest of the form (`.sheet__form.plan-on`).
    form.classList.toggle("plan-on", on);
    runnerBox.hidden = !on;
    if (on) paintRunner();
  };
  codingPlan.addEventListener("change", paintCodingPlan);

  /* ---- the device runner (subscription cards, and the Coding Plan) ---- */
  const runnerRoute = () => {
    if (!activeCard) return null;
    if (activeCard.kind === "runner") return activeCard.runner;
    if (activeCard.coding_plan && codingPlan.checked) return activeCard.coding_plan.runner;
    return null;
  };
  const runnerModels = () => {
    if (!activeCard) return [];
    if (activeCard.kind === "runner") return activeCard.models;
    return (activeCard.coding_plan && activeCard.coding_plan.models) || [];
  };
  const paintRunner = () => {
    const route = runnerRoute();
    if (!route) return;
    const role = runnerRole.value || "main";
    const existing = mine.find((r) => r.role === role);
    const ownsRoute = existing && existing.agent === route.agent && (existing.settings || "isolated") === route.settings;
    const models = runnerModels().map((m) => ({ ...m }));
    if (ownsRoute && existing.model && !models.some((m) => m.id === existing.model)) {
      models.unshift({ id: existing.model });
    }
    const picked = fillModelSelect(runnerModel, models, ownsRoute ? existing.model : undefined);
    runnerCustom.hidden = picked && runnerModel.value !== OTHER;
    if (runnerModel.value !== OTHER) runnerCustom.value = "";
    $("#runner-explain").textContent =
      `Jobs for you run on your own device through ${route.cli}, signed in with your own plan. ` +
      "Your sign-in never leaves your device: the brain hands the job to it, and only the answer comes back.";
    $("#runner-seen").textContent = runnersAvailable
      ? lastSeenText(lastSeen)
      : "Running jobs on your device isn't available right now.";
    const how = $("#runner-how");
    how.replaceChildren();
    how.append("Turn on ");
    const strong = document.createElement("strong");
    strong.textContent = "Run brain jobs with my subscription";
    how.appendChild(strong);
    how.append(" in the Living Brain Mac app, or run ");
    const code = document.createElement("code");
    code.textContent = runnerCommand(route.agent);
    how.appendChild(code);
    how.append(" on a machine where you are signed in.");
    runnerSave.disabled = !runnersAvailable;
    runnerSave.textContent = ownsRoute ? "Save" : "Run this job on my device";
  };
  runnerRole.addEventListener("change", paintRunner);
  runnerModel.addEventListener("change", () => {
    const other = runnerModel.value === OTHER;
    runnerCustom.hidden = !other;
    if (other) runnerCustom.focus();
  });
  runnerSave.addEventListener("click", async () => {
    const role = runnerRole.value;
    const chosen = runnerModel.value === OTHER ? runnerCustom.value.trim() : runnerModel.value;
    const value = runnerValue(activeCard, chosen, { codingPlan: activeCard.kind !== "runner" });
    if (!value || !roleInfo(role)) return;
    runnerSave.disabled = true;
    try {
      const outcome = await setSetting("model-runner", role, value);
      if (outcome.ok) {
        toast(`${roleInfo(role).label} now runs on your device.`, "ok");
        await load();
        paintSheet();
      } else {
        toast(runnerFailure(outcome), "bad");
      }
    } finally {
      runnerSave.disabled = !runnersAvailable;
    }
  });

  /* ---- the sheet ---- */
  const paintConnected = () => {
    const box = $("#sheet-connected");
    box.replaceChildren();
    const view = cardViews(rows, mine, jev).find((v) => v.id === activeCard.id);
    if (!view || !view.slots.length) return;
    const heading = document.createElement("h3");
    heading.textContent = "In use";
    box.appendChild(heading);
    const ul = document.createElement("ul");
    ul.className = "sheet__uses";
    for (const slot of view.slots) {
      const li = document.createElement("li");
      const row = rows.find((r) => r.role === slot.role);
      const text = document.createElement("span");
      const status = !slot.mine && row ? statusView(row) : null;
      text.textContent = slot.mine
        ? `${slot.label} · ${slot.model || "default model"} · on your device`
        : `${slot.label} · ${slot.model} · key ${maskedKey(row && row.key)}${status ? ` · ${status.label}` : ""}`;
      li.appendChild(text);
      const actions = document.createElement("span");
      actions.className = "sheet__use-actions";
      if (!slot.mine) {
        const test = document.createElement("button");
        test.type = "button";
        test.className = "btn-sm";
        test.textContent = "Test";
        test.setAttribute("data-sheet-action", "test");
        test.setAttribute("data-role", slot.role);
        test.setAttribute("aria-label", `Test the ${slot.label} connection`);
        actions.appendChild(test);
      }
      const off = document.createElement("button");
      off.type = "button";
      off.className = "btn-sm";
      off.textContent = slot.mine ? "Turn off" : "Disconnect";
      off.setAttribute("data-sheet-action", slot.mine ? "stop-runner" : "remove");
      off.setAttribute("data-role", slot.role);
      off.setAttribute("aria-label", `${off.textContent} ${slot.label}`);
      actions.appendChild(off);
      li.appendChild(actions);
      ul.appendChild(li);
    }
    box.appendChild(ul);
  };

  const paintSheet = () => {
    const card = activeCard;
    $("#sheet-name").textContent = card.name;
    $("#sheet-desc").textContent = card.detail || card.description;
    const logo = $("#sheet-logo");
    logo.setAttribute("data-logo", String(card.logo || "").replace(/^.*\//, "").replace(/\.svg$/, ""));
    const badges = $("#sheet-badges");
    badges.replaceChildren();
    for (const badge of card.badges || []) {
      const tag = document.createElement("span");
      tag.className = "mcard__badge";
      tag.setAttribute("data-badge", String(BADGES.indexOf(badge)));
      tag.textContent = badge;
      badges.appendChild(tag);
    }
    $("#sheet-protocol").textContent = card.kind === "custom" ? protocolLabel(null, checked(form, "wire")) : protocolLabel(card);
    paintConnected();
    const runner = card.kind === "runner";
    const classifier = card.kind === "classifier";
    form.hidden = runner || classifier;
    runnerBox.hidden = !runner;
    jevBox.hidden = !classifier;
    $("#provider-fieldset").hidden = card.kind !== "custom";
    if (classifier) paintJev();
    else if (runner) paintRunner();
    else paintCodingPlan();
  };

  /* ---- Jev, the optional triage classifier ---- */
  const paintJev = () => {
    const view = jevView(jev);
    const state = $("#jev-state");
    state.textContent = jev ? view.line : "Jev's state could not be loaded.";
    state.dataset.state = view.source;
    jevEnabled.checked = view.enabled;
    jevKey.value = "";
    jevKey.placeholder = view.hasKey ? `Stored: ${view.key} — paste a new key to replace it` : "Paste a TypeSafe API key";
    $("#jev-key-hint").textContent = view.hasKey
      ? `This workspace's own key (${view.key}) is used ahead of the deployment's. It is stored encrypted and never shown again.`
      : view.deploymentKey
        ? "Optional: this deployment already has a key. Add the workspace's own to use it instead."
        : "Stored encrypted for this workspace; only its last four characters are ever shown.";
    jevRemoveKey.hidden = !view.hasKey;
    jevModel.value = view.model;
    jevModel.placeholder = view.defaultModel;
    $("#jev-model-hint").textContent = `Default: ${view.defaultModel}, the version the adapter pins. Leave it to follow the default.`;
    $("#jev-privacy").textContent = JEV_PRIVACY;
    fieldError("jev-key", "", jevKey);
    fieldError("jev-model", "", jevModel);
    jevSave.disabled = !jev;
    jevTest.disabled = !jev || !view.available;
  };
  // The token savers live on another tab: close the sheet on the way there.
  $("#jev-savers").addEventListener("click", () => {
    if (sheet.open) sheet.close();
  });
  const jevForm = () => ({ enabled: jevEnabled.checked, apiKey: jevKey.value, model: jevModel.value });
  jevSave.addEventListener("click", async () => {
    const check = jevChange(jevForm(), jev);
    fieldError("jev-key", check.errors.apiKey, jevKey);
    fieldError("jev-model", check.errors.model, jevModel);
    if (!check.ok) return;
    if (!Object.keys(check.value).length) {
      toast("Nothing to save.", "ok");
      return;
    }
    jevSave.disabled = true;
    try {
      const outcome = await setSetting("jev", "settings", check.value);
      if (outcome.ok) {
        jev = outcome.data || jev;
        toast(jevView(jev).on ? "Jev saved — it is on." : "Jev saved — it is off.", "ok");
      } else {
        toast(`Not saved: ${outcome.message}`, "bad");
      }
    } finally {
      jevKey.value = "";
      renderModelCards(cardsBox, cardViews(rows, mine, jev));
      if (sheet.open) paintSheet();
    }
  });
  jevTest.addEventListener("click", async () => {
    jevTest.disabled = true;
    jevTest.textContent = "Testing…";
    try {
      const outcome = await setSetting("jev-test", "test", jevTestBody(jevForm(), jev));
      if (outcome.ok) toast(jevTestMessage(outcome.data), "ok");
      else toast(`Test failed: ${outcome.message}`, "bad");
    } finally {
      jevTest.disabled = false;
      jevTest.textContent = "Test";
    }
  });
  jevRemoveKey.addEventListener("click", async () => {
    const ok = await confirmAction({
      title: "Remove the workspace's TypeSafe key?",
      body: "The key is deleted. Jev then runs on the deployment's key if it has one, and is off if not.",
      confirm: "Remove",
    });
    if (!ok) return;
    jevRemoveKey.disabled = true;
    const outcome = await setSetting("jev", "key", null);
    jevRemoveKey.disabled = false;
    if (outcome.ok) {
      jev = outcome.data || jev;
      toast("Workspace key removed.", "ok");
    } else {
      toast(`Not removed: ${outcome.message}`, "bad");
    }
    renderModelCards(cardsBox, cardViews(rows, mine, jev));
    if (sheet.open) paintSheet();
  });

  /** Opens a card's sheet, for `role` when one is named (a slot's Change). */
  const openCard = (cardId, role) => {
    activeCard = cardInfo(cardId) || cardInfo("custom");
    codingPlan.checked = false;
    const runnerRow = role ? mine.find((r) => r.role === role) : null;
    if (activeCard.coding_plan && runnerRow && runnerRow.settings === "user") codingPlan.checked = true;
    form.reset();
    codingPlan.checked = Boolean(activeCard.coding_plan && runnerRow && runnerRow.settings === "user");
    apiKey.type = "password";
    $("#toggle-key").textContent = "Show";
    search.value = "";
    listed = { provider: null, models: [] };
    const targetRole = role && roleInfo(role) ? role : "main";
    roleSelect.value = targetRole;
    runnerRole.value = targetRole;
    if (activeCard.kind !== "runner" && activeCard.kind !== "classifier") {
      const row = rows.find((r) => r.role === targetRole && cardForConnection(r) === activeCard);
      const providerId = activeCard.kind === "custom" ? (row ? row.provider : "custom") : activeCard.provider;
      renderList();
      applyProvider(providerId);
      if (row) {
        const stored = String(row.base_url || "").replace(/\/+$/, "");
        const provider = providerForConnection(row);
        if (provider.id === "custom") {
          baseUrl.value = stored;
          const wire = form.querySelector(`input[name="wire"][value="${row.wire}"]`);
          const auth = form.querySelector(`input[name="auth"][value="${row.auth}"]`);
          if (wire) wire.checked = true;
          if (auth) auth.checked = true;
        } else if (stored && stored !== String(provider.base_url).replace(/\/+$/, "")) {
          editing = true;
          paintEndpoint();
          baseUrl.value = stored;
        }
        paintModels(row.model || "");
      }
      submit.textContent = row ? "Save and test" : "Connect";
    }
    paintSheet();
    if (typeof sheet.showModal === "function") sheet.showModal();
    else sheet.setAttribute("open", "");
  };

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const check = validateConnect(readForm());
    const first = showErrors(check.errors);
    if (!check.ok) {
      if (first) first.focus();
      return;
    }
    const label = submit.textContent;
    submit.disabled = true;
    submit.textContent = "Testing the connection…";
    try {
      const outcome = await setSetting("models", check.role, check.value);
      if (outcome.ok) {
        toast(`${roleInfo(check.role).label} model connected.`, "ok");
        apiKey.value = "";
        await load();
        paintSheet();
      } else {
        toast(`Not connected: ${outcome.message}`, "bad");
      }
    } finally {
      submit.disabled = false;
      if (submit.textContent === "Testing the connection…") submit.textContent = label;
    }
  });

  /** Turns a device runner off for a role. */
  const stopRunner = async (role) => {
    const outcome = await setSetting("model-runner", role, null);
    if (outcome.ok) toast(`${roleInfo(role).label} no longer runs on your device.`, "ok");
    else toast(runnerFailure(outcome), "bad");
    return outcome.ok;
  };

  /** Disconnect, Test and Turn off — the sheet's and the slots' buttons. */
  const slotAction = async (action, role, button) => {
    const info = roleInfo(role);
    if (!info) return;
    if (action === "stop-runner") {
      button.disabled = true;
      if (await stopRunner(role)) await load();
      button.disabled = false;
    } else if (action === "remove") {
      const ok = await confirmAction({
        title: `Disconnect the ${info.label} model?`,
        body:
          role === "main"
            ? "Its key is deleted. Until you connect another, the managed model answers for every job."
            : "Its key is deleted. This job uses Main while Main is set, and the managed model when it is not.",
        confirm: "Disconnect",
      });
      if (!ok) return;
      button.disabled = true;
      const outcome = await setSetting("models", role, null);
      if (outcome.ok) {
        toast(`${info.label} model disconnected.`, "ok");
        await load();
      } else {
        button.disabled = false;
        toast(`Not disconnected: ${outcome.message}`, "bad");
      }
    } else if (action === "test") {
      const row = rows.find((r) => r.role === role);
      if (!row) return;
      button.disabled = true;
      button.textContent = "Testing…";
      const outcome = await setSetting("model-switch", role, { model: row.model });
      if (outcome.ok) {
        await load();
        const fresh = rows.find((r) => r.role === role);
        const status = fresh ? statusView(fresh) : null;
        toast(
          status && status.state === "live" ? `${info.label} works.` : `${info.label}: ${status ? `${status.label}. ${status.detail}` : "checked"}`,
          status && status.state === "live" ? "ok" : "warn",
        );
      } else {
        toast(`Test failed: ${outcome.message}`, "bad");
      }
      button.disabled = false;
      button.textContent = "Test";
    }
    if (sheet.open) paintSheet();
  };

  $("#sheet-connected").addEventListener("click", (event) => {
    const button = event.target.closest("[data-sheet-action]");
    if (button) slotAction(button.dataset.sheetAction, button.dataset.role, button);
  });
  cardsBox.addEventListener("click", (event) => {
    const button = event.target.closest("[data-card]");
    if (button) openCard(button.dataset.card);
  });
  slots.addEventListener("click", async (event) => {
    const button = event.target.closest("[data-action]");
    if (!button) return;
    const role = button.dataset.role;
    if (button.dataset.action === "remove" || button.dataset.action === "stop-runner") {
      await slotAction(button.dataset.action, role, button);
      return;
    }
    const runner = mine.find((r) => r.role === role);
    const row = rows.find((r) => r.role === role);
    const card = runner ? runnerCardFor(runner) : row ? cardForConnection(row) : cardInfo("anthropic");
    openCard(card ? card.id : "anthropic", role);
  });

  /* ---- the "Main model" picker ---- */
  const paintMainPick = () => {
    choices = mainModelChoices(rows, mine, { runnersAvailable });
    mainPick.replaceChildren();
    const groups = new Map();
    for (const option of choices.options) {
      if (!groups.has(option.group)) {
        const group = document.createElement("optgroup");
        group.label = option.group;
        groups.set(option.group, group);
        mainPick.appendChild(group);
      }
      const node = document.createElement("option");
      node.value = option.value;
      node.textContent = option.label;
      groups.get(option.group).appendChild(node);
    }
    mainPick.value = choices.current;
    const view = slotView(rows, "main", mine);
    mainLine.textContent = view.set ? view.line : "Nothing connected: the managed model answers.";
  };
  mainPick.addEventListener("change", async () => {
    const option = choices.options.find((o) => o.value === mainPick.value);
    if (!option) return;
    mainPick.disabled = true;
    try {
      const action = option.action;
      if (action.dropRunner && !(await stopRunner("main"))) return;
      if (action.kind === "switch") {
        const main = rows.find((r) => r.role === "main");
        if (main && main.model !== action.model) {
          mainLine.textContent = "Testing the new model…";
          const outcome = await setSetting("model-switch", "main", { model: action.model });
          if (outcome.ok) toast("Main model switched.", "ok");
          else toast(`Not switched: ${outcome.message}`, "bad");
        }
      } else if (action.kind === "runner") {
        const outcome = await setSetting("model-runner", "main", {
          kind: "local_runner",
          agent: action.agent,
          model: action.model,
          settings: action.settings,
        });
        if (outcome.ok) toast("Main now runs on your device.", "ok");
        else toast(runnerFailure(outcome), "bad");
      }
    } finally {
      mainPick.disabled = false;
      await load();
    }
  });

  const load = async () => {
    const [connections, own, classifier] = await Promise.allSettled([
      api("/v1/models"),
      api("/v1/models/mine"),
      api("/v1/models/jev"),
    ]);
    // An older server without the Jev routes: the card shows no state.
    jev = classifier.status === "fulfilled" ? classifier.value : null;
    if (connections.status === "fulfilled") {
      rows = Array.isArray(connections.value) ? connections.value : [];
      notice(modelsStatus, "");
    } else {
      rows = [];
      notice(modelsStatus, `Could not load your models: ${connections.reason.message}`, "warn");
    }
    if (own.status === "fulfilled") {
      runnersAvailable = true;
      const value = own.value || {};
      mine = Array.isArray(value.runners) ? value.runners : [];
      lastSeen = mine.map((r) => r.last_seen_at).filter(Boolean).sort().pop() || null;
    } else {
      // A server without the device-runner routes yet answers 404: that is
      // "not available", not an error worth a banner.
      runnersAvailable = !(own.reason instanceof ApiError && (own.reason.status === 404 || own.reason.status === 405));
      mine = [];
      lastSeen = null;
    }
    renderModelCards(cardsBox, cardViews(rows, mine, jev));
    renderModelSlots(slots, slotViews(rows, mine));
    paintMainPick();
  };

  await load();
}

/** The note under a curated model list when the provider could not be asked. */
const DISCOVER_FALLBACK_NOTE = "This provider doesn't publish a model list; showing known models.";
const DISCOVER_UNREACHABLE_NOTE = "Couldn't get this provider's model list right now; showing known models.";

/** A device-runner change that did not take, in words. */
function runnerFailure(outcome) {
  if (outcome.status === 404 || outcome.status === 405) {
    return "Running jobs on your device isn't available right now.";
  }
  return `Not saved: ${outcome.message}`;
}

/* --- api tokens ------------------------------------------------------ */

/**
 * The token section. Three rules, all about where a secret may live: the list
 * is rendered from `tokenViews`, which cannot carry a value; the value a create
 * returns is written once into `#token-value`, beside "you won't see it
 * again"; and dismissing it — or leaving the page — takes it back out.
 */
function wireTokens() {
  const status = $("#tokens-status");
  const rows = $("#token-rows");
  const reveal = $("#token-reveal");
  const value = $("#token-value");
  const form = $("#token-form");
  const name = $("#token-name");
  const submit = $("#create-token");

  choiceCards(
    $("#scope-choices"),
    "token-scope",
    TOKEN_SCOPES.map((s) => ({ value: s.value, label: s.label, detail: s.detail })),
    "",
  );

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
      notice(status, tokenErrorMessage(error, "load your tokens"), "warn");
    }
  };

  name.addEventListener("input", () => fieldError("tokenName", "", name));

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    let request;
    try {
      request = tokenCreateRequest(name.value, checked(form, "token-scope"));
    } catch {
      fieldError("tokenName", "Give the token a name.", name);
      name.focus();
      return;
    }
    submit.disabled = true;
    try {
      const created = await api(request.path, {
        method: request.method,
        body: request.body,
      });
      revealToken(value, createdTokenView(created));
      reveal.hidden = false;
      reveal.scrollIntoView({ behavior: "smooth", block: "center" });
      toast("Token created. Copy it now.", "ok");
      form.reset();
      await load();
    } catch (error) {
      toast(tokenErrorMessage(error, "create the token"), "bad");
    } finally {
      submit.disabled = false;
    }
  });

  // One handler for the table: each row's button carries its own prefix, so the
  // list can be re-rendered without re-attaching anything.
  rows.addEventListener("click", async (event) => {
    const button = event.target.closest("[data-prefix]");
    if (!button) return;
    const label = button.dataset.name;
    const ok = await confirmAction({
      title: `Revoke "${label}"?`,
      body: "Anything using this token stops working at once. This cannot be undone.",
      confirm: "Revoke token",
    });
    if (!ok) return;
    try {
      const request = tokenRevokeRequest(button.dataset.prefix);
      await api(request.path, { method: request.method });
      toast(`Revoked ${label}.`, "ok");
      await load();
    } catch (error) {
      toast(tokenErrorMessage(error, "revoke the token"), "bad");
    }
  });

  $("#copy-token").addEventListener("click", async (event) => {
    const button = event.currentTarget;
    try {
      await navigator.clipboard.writeText(value.textContent);
      button.textContent = "Copied";
      setTimeout(() => {
        button.textContent = "Copy";
      }, 1500);
    } catch {
      // Clipboard access can be refused; say where the value is rather than
      // pretending the copy worked.
      toast("Copying was blocked. Select the token and copy it by hand.", "warn");
    }
  });

  $("#dismiss-token").addEventListener("click", () => {
    forget();
    toast("The token is gone from this page.", "ok");
  });

  // Leaving the page is a dismissal too.
  window.addEventListener("pagehide", forget);

  return load();
}

/* --- members ---------------------------------------------------------- */

/**
 * The members section (ADR 0004): the roster for everyone, the invite form and
 * open invitations for owners and admins, and the workspace switcher. The
 * server decides every permission; the page only hides what would be refused.
 */
async function wireMembers() {
  const status = $("#members-status");
  const rows = $("#member-rows");
  const admin = $("#invite-admin");
  const form = $("#invite-form");
  const email = $("#invite-email");
  const role = $("#invite-role");
  const invites = $("#invite-rows");
  const spaces = $("#workspace-list");
  let me = null;

  const send = (request) =>
    api(request.path, request.body === undefined
      ? { method: request.method }
      : { method: request.method, body: request.body });

  // A changed name or picture (`wireProfile`) shows in the roster at once.
  document.addEventListener("lb-profile", () => {
    load();
  });

  const load = async () => {
    try {
      me = await api("/v1/workspaces/me");
      renderMemberRows(rows, memberViews(await send(membersRequest())), me);
      const manage = canManage(me);
      admin.hidden = !manage;
      if (manage) {
        role.replaceChildren();
        for (const option of assignableRoles(me)) {
          const el = document.createElement("option");
          el.value = option.value;
          el.textContent = option.label;
          role.appendChild(el);
        }
        renderInviteRows(invites, inviteViews(await send(invitesRequest())));
      }
      renderWorkspaces(spaces, await send(workspacesRequest()));
      notice(status, "");
    } catch (error) {
      rows.replaceChildren();
      notice(status, memberErrorMessage(error, "load the members"), "warn");
    }
  };

  email.addEventListener("input", () => fieldError("inviteEmail", "", email));

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    let request;
    try {
      request = inviteRequest(email.value, role.value);
    } catch {
      fieldError("inviteEmail", "Enter an email address.", email);
      email.focus();
      return;
    }
    try {
      await send(request);
      toast(`Invitation sent to ${request.body.email}.`, "ok");
      form.reset();
      await load();
    } catch (error) {
      toast(memberErrorMessage(error, "send the invitation"), "bad");
    }
  });

  rows.addEventListener("change", async (event) => {
    const select = event.target.closest("[data-role-for]");
    if (!select) return;
    try {
      await send(roleChangeRequest(select.dataset.roleFor, select.value));
      toast("Role changed.", "ok");
    } catch (error) {
      toast(memberErrorMessage(error, "change the role"), "bad");
    }
    await load();
  });

  rows.addEventListener("click", async (event) => {
    const button = event.target.closest("[data-remove]");
    if (!button) return;
    const leaving = button.dataset.self === "1";
    const ok = await confirmAction({
      title: leaving ? "Leave this workspace?" : `Remove ${button.dataset.name}?`,
      body: leaving
        ? "You will be signed out, and only an invitation brings you back."
        : "They are signed out everywhere and their API tokens stop working.",
      confirm: leaving ? "Leave" : "Remove",
    });
    if (!ok) return;
    try {
      await send(removeRequest(button.dataset.remove));
      if (leaving) {
        location.assign("/");
        return;
      }
      toast(`Removed ${button.dataset.name}.`, "ok");
      await load();
    } catch (error) {
      toast(memberErrorMessage(error, leaving ? "leave" : "remove them"), "bad");
    }
  });

  invites.addEventListener("click", async (event) => {
    const button = event.target.closest("[data-withdraw]");
    if (!button) return;
    try {
      await send(withdrawRequest(button.dataset.withdraw));
      toast(`Withdrew the invitation to ${button.dataset.email}.`, "ok");
      await load();
    } catch (error) {
      toast(memberErrorMessage(error, "withdraw the invitation"), "bad");
    }
  });

  const createForm = $("#workspace-create");
  const createName = $("#workspace-create-name");
  if (createForm && createName) {
    createForm.addEventListener("submit", async (event) => {
      event.preventDefault();
      let request;
      try {
        request = createWorkspaceRequest(createName.value);
      } catch (error) {
        fieldError("workspaceCreate", error.message, createName);
        return;
      }
      fieldError("workspaceCreate", "", createName);
      const submit = $("#workspace-create-submit");
      if (submit) submit.disabled = true;
      try {
        await send(request);
        location.assign("/");
      } catch (error) {
        fieldError("workspaceCreate", memberErrorMessage(error, "create the workspace"), createName);
        if (submit) submit.disabled = false;
      }
    });
  }

  spaces.addEventListener("click", async (event) => {
    const button = event.target.closest("[data-switch]");
    if (!button) return;
    try {
      await send(switchRequest(button.dataset.switch));
      location.assign("/");
    } catch (error) {
      toast(memberErrorMessage(error, "switch workspace"), "bad");
    }
  });

  return load();
}

/**
 * Deleting the workspace (ADR 0004, "Deleting a workspace"): owners only, and
 * only where the person has another workspace to land in. The button opens a
 * panel that says what goes; the red button stays disabled until the typed
 * name is the workspace's name exactly. Success lands on `/`, where the
 * session already points at the next workspace.
 */
async function wireDeleteWorkspace() {
  const zone = $("#workspace-danger");
  const only = $("#workspace-danger-only");
  const open = $("#workspace-delete-open");
  const panel = $("#workspace-delete-panel");
  const nameEl = $("#workspace-delete-name");
  const input = $("#workspace-delete-confirm");
  const submit = $("#workspace-delete-submit");
  const status = $("#workspace-delete-status");
  if (!zone || !open || !panel || !input || !submit) return;
  let name = "";

  const paint = async () => {
    try {
      const me = await api("/v1/workspaces/me");
      const list = await api(workspacesRequest().path);
      const state = deleteZone(me, list);
      zone.hidden = !state.owner;
      if (only) only.hidden = !state.onlyWorkspace;
      open.hidden = !state.show;
      if (!state.show) panel.hidden = true;
      name = String((me.workspace && me.workspace.name) || "");
      if (nameEl) nameEl.textContent = name;
      input.value = "";
      submit.disabled = true;
    } catch (error) {
      zone.hidden = true;
    }
  };
  document.addEventListener("lb-profile", paint);

  open.addEventListener("click", () => {
    panel.hidden = false;
    input.focus();
  });
  input.addEventListener("input", () => {
    submit.disabled = !deleteConfirmMatches(input.value, name);
  });
  submit.addEventListener("click", async () => {
    if (!deleteConfirmMatches(input.value, name)) return;
    submit.disabled = true;
    notice(status, "");
    try {
      const request = deleteWorkspaceRequest(input.value);
      await api(request.path, { method: request.method, body: request.body });
      location.assign("/");
    } catch (error) {
      notice(status, deleteErrorMessage(error), "bad");
      submit.disabled = !deleteConfirmMatches(input.value, name);
    }
  });

  return paint();
}

/* --- your profile and the workspace's -------------------------------- */

/**
 * One picture editor: a square you click or drop an image on. Picking one
 * turns the square into a cropper — drag to move, the slider or the wheel to
 * zoom, a circle showing what a round avatar keeps — with Use / Cancel; with
 * a stored picture, Remove. `target` is `"me"` or `"workspace"`; `after`
 * runs once the server has the change.
 */
function wireAvatarEditor({ prefix, target, status, what, after }) {
  const picker = $(`#${prefix}-picker`);
  const avatar = $(`#${prefix}-avatar`);
  const preview = $(`#${prefix}-preview`);
  const guide = $(`#${prefix}-guide`);
  const zoomRow = $(`#${prefix}-zoom-row`);
  const zoom = $(`#${prefix}-zoom`);
  const file = $(`#${prefix}-file`);
  const save = $(`#${prefix}-avatar-save`);
  const cancel = $(`#${prefix}-avatar-cancel`);
  const remove = $(`#${prefix}-avatar-remove`);
  if (!picker || !avatar || !preview || !file || !save || !cancel || !remove) return { paint() {}, enable() {} };
  let image = null;
  let view = null;
  let person = {};
  let enabled = true;

  const draw = () => {
    if (!image) return;
    view = cropRect(image.width, image.height, view || {});
    drawCrop(preview, image, view);
    if (zoom) zoom.value = String(view.zoom);
  };
  const showCrop = (on) => {
    preview.hidden = !on;
    avatar.hidden = on;
    if (guide) guide.hidden = !on;
    if (zoomRow) zoomRow.hidden = !on;
    picker.classList.toggle("is-cropping", on);
    picker.setAttribute("aria-label", on ? "Drag to move the picture; arrow keys move it, + and - zoom" : picker.dataset.label);
    save.hidden = !on;
    cancel.hidden = !on;
    remove.hidden = on || !person.url;
  };
  const reset = () => {
    if (image && typeof image.close === "function") image.close();
    image = null;
    view = null;
    file.value = "";
    fieldError(`${prefix}File`, "", picker);
    showCrop(false);
  };
  picker.dataset.label = picker.getAttribute("aria-label") || "Change the picture";

  const take = async (picked) => {
    if (!enabled) return;
    const problem = avatarFileError(picked);
    if (problem) {
      fieldError(`${prefix}File`, problem, picker);
      return;
    }
    try {
      const next = await readImage(picked);
      reset();
      image = next;
      view = { zoom: 1 };
      showCrop(true);
      draw();
    } catch (error) {
      reset();
      fieldError(`${prefix}File`, error.message || "The image could not be read.", picker);
    }
  };

  file.addEventListener("change", () => take(file.files && file.files[0]));
  picker.addEventListener("click", () => {
    if (enabled && !image) file.click();
  });

  // Drop an image on the square, whether or not one is being cropped.
  picker.addEventListener("dragover", (event) => {
    if (!enabled) return;
    event.preventDefault();
    picker.classList.add("is-over");
  });
  picker.addEventListener("dragleave", () => picker.classList.remove("is-over"));
  picker.addEventListener("drop", (event) => {
    picker.classList.remove("is-over");
    if (!enabled) return;
    event.preventDefault();
    const dropped = event.dataTransfer && event.dataTransfer.files && event.dataTransfer.files[0];
    if (dropped) take(dropped);
  });

  // Drag to move the crop.
  let drag = null;
  picker.addEventListener("pointerdown", (event) => {
    if (!image) return;
    drag = { x: event.clientX, y: event.clientY };
    picker.setPointerCapture(event.pointerId);
  });
  picker.addEventListener("pointermove", (event) => {
    if (!drag || !image) return;
    const shown = picker.getBoundingClientRect().width;
    view = panCrop(image.width, image.height, view, event.clientX - drag.x, event.clientY - drag.y, shown);
    drag = { x: event.clientX, y: event.clientY };
    draw();
  });
  const endDrag = () => {
    drag = null;
  };
  picker.addEventListener("pointerup", endDrag);
  picker.addEventListener("pointercancel", endDrag);

  // Zoom about the centre: the slider, the wheel, and + / - on the keyboard.
  const zoomTo = (value) => {
    if (!image) return;
    view = { ...view, zoom: value };
    draw();
  };
  if (zoom) zoom.addEventListener("input", () => zoomTo(Number(zoom.value)));
  picker.addEventListener(
    "wheel",
    (event) => {
      if (!image) return;
      event.preventDefault();
      zoomTo(view.zoom * Math.exp(-event.deltaY / 400));
    },
    { passive: false },
  );
  picker.addEventListener("keydown", (event) => {
    if (!enabled) return;
    if (!image) {
      if (event.key === "Enter" || event.key === " ") {
        event.preventDefault();
        file.click();
      }
      return;
    }
    const step = 8;
    const moves = { ArrowLeft: [step, 0], ArrowRight: [-step, 0], ArrowUp: [0, step], ArrowDown: [0, -step] };
    if (moves[event.key]) {
      event.preventDefault();
      const shown = picker.getBoundingClientRect().width;
      view = panCrop(image.width, image.height, view, moves[event.key][0], moves[event.key][1], shown);
      draw();
    } else if (event.key === "+" || event.key === "=") {
      zoomTo(view.zoom * 1.1);
    } else if (event.key === "-") {
      zoomTo(view.zoom / 1.1);
    } else if (event.key === "Escape") {
      reset();
    }
  });

  save.addEventListener("click", async () => {
    if (!image) return;
    save.disabled = true;
    try {
      // The upload is drawn fresh at `AVATAR_SIDE`, from the same view.
      const out = document.createElement("canvas");
      out.width = AVATAR_SIDE;
      out.height = AVATAR_SIDE;
      drawCrop(out, image, view);
      const request = avatarUploadRequest(await encodeAvatar(out), target);
      await api(request.path, { method: request.method, raw: request.raw, contentType: request.contentType });
      reset();
      toast("Picture saved.", "ok");
      await after();
    } catch (error) {
      if (error && error.status) notice(status, profileErrorMessage(error, `save ${what} picture`), "bad");
      else fieldError(`${prefix}File`, error.message || "The image could not be prepared.", picker);
    } finally {
      save.disabled = false;
    }
  });

  cancel.addEventListener("click", reset);

  remove.addEventListener("click", async () => {
    const ok = await confirmAction({
      title: "Remove the picture?",
      body: "Initials take its place.",
      confirm: "Remove",
    });
    if (!ok) return;
    try {
      const request = avatarRemoveRequest(target);
      await api(request.path, { method: request.method });
      toast("Picture removed.", "ok");
      await after();
    } catch (error) {
      notice(status, profileErrorMessage(error, `remove ${what} picture`), "bad");
    }
  });

  return {
    /** Shows the stored picture (or initials) and whether Remove applies. */
    paint(next) {
      person = next;
      paintAvatar(avatar, next);
      if (preview.hidden) remove.hidden = !next.url;
    },
    /** Read-only for someone who may not change it. */
    enable(on) {
      enabled = on;
      picker.setAttribute("aria-disabled", on ? "false" : "true");
      picker.tabIndex = on ? 0 : -1;
      if (!on) {
        reset();
        remove.hidden = true;
      }
    },
  };
}

/**
 * Settings → Account (your name and picture here) and the head of the
 * Members tab (the workspace's, for owners and admins). Each change re-reads
 * `/v1/workspaces/me` and repaints the top bar and the roster from it.
 */
async function wireProfile() {
  const status = $("#profile-status");
  const form = $("#profile-form");
  const name = $("#profile-name");
  const nameHint = $("#profile-name-hint");
  const nameReset = $("#profile-name-reset");
  const wsStatus = $("#workspace-status");
  const wsForm = $("#workspace-form");
  const wsName = $("#workspace-name");
  const wsSave = $("#workspace-save");
  const wsReadonly = $("#workspace-readonly");
  if (!form || !name) return;
  let view = null;

  const refresh = async () => {
    const me = await api("/v1/workspaces/me");
    view = profileView(me);
    name.value = view.displayName || view.name;
    if (nameHint) {
      nameHint.textContent = view.displayName
        ? "The name you chose for this workspace. Each workspace keeps its own."
        : "1 to 64 characters. Each workspace keeps its own.";
    }
    if (nameReset) nameReset.hidden = !view.displayName;
    mine.paint({ name: view.name, id: view.userId, url: view.avatarUrl });
    if (wsName) {
      wsName.value = view.workspaceName;
      wsName.readOnly = !view.canEditWorkspace;
    }
    if (wsSave) wsSave.hidden = !view.canEditWorkspace;
    if (wsReadonly) wsReadonly.hidden = view.canEditWorkspace;
    theirs.paint({ name: view.workspaceName, id: view.workspaceId, url: view.workspaceAvatarUrl });
    theirs.enable(view.canEditWorkspace);
    if (shell) shell.applySession({ kind: "signed-in", me });
    return me;
  };
  const changed = async () => {
    await refresh();
    document.dispatchEvent(new CustomEvent("lb-profile"));
  };

  const mine = wireAvatarEditor({ prefix: "profile", target: "me", status, what: "your", after: changed });
  const theirs = wireAvatarEditor({
    prefix: "workspace",
    target: "workspace",
    status: wsStatus,
    what: "the workspace's",
    after: changed,
  });

  name.addEventListener("input", () => fieldError("profileName", "", name));
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const problem = nameError(name.value);
    if (problem) {
      fieldError("profileName", problem, name);
      name.focus();
      return;
    }
    try {
      const request = displayNameRequest(name.value);
      await api(request.path, { method: request.method, body: request.body });
      toast("Name saved.", "ok");
      await changed();
    } catch (error) {
      notice(status, profileErrorMessage(error, "save your name"), "bad");
    }
  });
  if (nameReset) {
    nameReset.addEventListener("click", async () => {
      try {
        const request = displayNameRequest(null);
        await api(request.path, { method: request.method, body: request.body });
        toast("Back to your sign-in name.", "ok");
        await changed();
      } catch (error) {
        notice(status, profileErrorMessage(error, "reset your name"), "bad");
      }
    });
  }

  if (wsForm && wsName) {
    wsName.addEventListener("input", () => fieldError("workspaceName", "", wsName));
    wsForm.addEventListener("submit", async (event) => {
      event.preventDefault();
      if (!view || !view.canEditWorkspace) return;
      const problem = nameError(wsName.value);
      if (problem) {
        fieldError("workspaceName", problem, wsName);
        wsName.focus();
        return;
      }
      try {
        const request = workspaceNameRequest(wsName.value);
        await api(request.path, { method: request.method, body: request.body });
        toast("Workspace renamed.", "ok");
        await changed();
      } catch (error) {
        notice(wsStatus, profileErrorMessage(error, "rename the workspace"), "bad");
      }
    });
  }

  try {
    await refresh();
    notice(status, "");
  } catch (error) {
    notice(status, profileErrorMessage(error, "load your profile"), "warn");
  }
}

/* --- usage & limits --------------------------------------------------- */

/**
 * The Usage & limits section: every budget with its usage bar for everyone,
 * the presets and inputs for owners and admins. A save sends only what
 * changed, through the settings choke point; the server checks every range
 * and every role, and the page shows its one-sentence refusal.
 */
async function wireBudgets() {
  const status = $("#limits-status");
  const body = $("#limits-body");
  const form = $("#limits-form");
  const actions = $("#limits-actions");
  const readonly = $("#limits-readonly");
  const reset = $("#limits-reset");
  if (!body || !form) return undefined;
  let view = null;

  const clearErrors = () => {
    for (const line of $$("[data-budget-error]", body)) line.textContent = "";
  };

  /** Marks the preset the inputs now match (or Custom). */
  const markPreset = () => {
    const id = matchPreset(readInputs(body, true));
    for (const radio of $$('input[name="budget-preset"]', body)) radio.checked = radio.value === id;
  };

  const paint = (response) => {
    view = budgetView(response);
    renderBudgets(body, view);
    actions.hidden = !view.editable;
    readonly.hidden = view.editable;
    markPreset();
  };

  const load = async () => {
    try {
      const request = budgetsRequest();
      paint(await api(request.path, { method: request.method }));
      notice(status, "");
    } catch (error) {
      body.replaceChildren();
      actions.hidden = true;
      notice(status, budgetErrorMessage(error, "load the limits"), "warn");
    }
  };

  body.addEventListener("change", (event) => {
    const radio = event.target.closest('input[name="budget-preset"]');
    if (!radio || radio.value === "custom") return;
    const values = presetInputs(radio.value);
    for (const [name, value] of Object.entries(values || {})) {
      const input = body.querySelector(`[data-budget="${name}"]`);
      if (input) input.value = value;
    }
    clearErrors();
  });

  body.addEventListener("input", (event) => {
    if (!event.target.closest("[data-budget]")) return;
    clearErrors();
    markPreset();
  });

  reset.addEventListener("click", () => {
    if (view) {
      renderBudgets(body, view);
      markPreset();
    }
    notice(status, "");
  });

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    if (!view || !view.editable) return;
    clearErrors();
    let patch;
    try {
      const paused = $("#budget-paused");
      patch = budgetPatch(
        view,
        readInputs(body),
        paused ? paused.checked : undefined,
        readSavers(body),
      );
    } catch (error) {
      if (error instanceof FieldError) {
        const line = body.querySelector(`[data-budget-error="${error.field}"]`);
        if (line) line.textContent = error.message;
        const input = body.querySelector(`[data-budget="${error.field}"]`);
        if (input) input.focus();
        return;
      }
      throw error;
    }
    if (!patch) {
      toast("Nothing to save — the limits are as they were.", "ok");
      return;
    }
    const outcome = await setSetting("budgets", "limits", patch);
    if (outcome.ok) {
      toast("Limits saved.", "ok");
      await load();
    } else {
      const message = budgetErrorMessage(
        { status: outcome.status, message: outcome.message, body: { detail: outcome.message } },
        "save the limits",
      );
      notice(status, message, "bad");
    }
  });

  return load();
}

/* --- agent accounts, access, audit (ADR 0004) ------------------------ */

/**
 * The three access sections. The server decides every permission; the page
 * hides only what would be refused. An agent key is written into
 * `#agent-key-value` by `revealAgentKey` and nowhere else, and dismissing it —
 * or leaving the page — takes it back out.
 */
async function wireAccess() {
  const send = (request) =>
    api(request.path, request.body === undefined
      ? { method: request.method }
      : { method: request.method, body: request.body });

  const agentsStatus = $("#agents-status");
  const agentRows = $("#agent-rows");
  const keysBox = $("#agent-keys");
  const keysTitle = $("#agent-keys-title");
  const keyRows = $("#agent-key-rows");
  const keyForm = $("#agent-key-form");
  const reveal = $("#agent-key-reveal");
  const keyValue = $("#agent-key-value");
  const agentForm = $("#agent-form");
  const accessStatus = $("#access-status");
  const grantRows = $("#grant-rows");
  const grantForm = $("#grant-form");
  const grantPreviewLine = $("#grant-preview");
  const grantSubmit = $("#grant-submit");
  const grantFilterWho = $("#grant-filter-who");
  const grantFilterWhere = $("#grant-filter-where");
  const grantFilterWhat = $("#grant-filter-what");
  const grantSelectAll = $("#grant-select-all");
  const grantSelectedLine = $("#grant-selected");
  const grantRevokeSelected = $("#grant-revoke-selected");
  const agentSelectAll = $("#agent-select-all");
  const agentSelectedLine = $("#agent-selected");
  const agentDisableSelected = $("#agent-disable-selected");
  const agentEnableSelected = $("#agent-enable-selected");
  const auditSection = $("#audit");
  const auditRows = $("#audit-rows");
  const auditMore = $("#audit-more");
  let me = null;
  let members = [];
  let agents = [];
  let openAgent = null;
  let auditCursor = "";
  let grants = [];
  let grantsShown = [];
  let grantSelection = new Set();
  let agentSelection = new Set();
  const grantFilter = { who: "", where: "", what: "" };

  const forget = () => {
    clearAgentKey(keyValue);
    reveal.hidden = true;
  };
  const showKey = (response) => {
    revealAgentKey(keyValue, mintedKey(response));
    reveal.hidden = false;
    reveal.scrollIntoView({ behavior: "smooth", block: "center" });
    toast("Key created. Copy it now.", "ok");
  };

  const whoPicker = createMultiSelect($("#grant-who"), {
    label: "who gets access",
    placeholder: "Search people, agents and roles…",
    onChange: () => previewGrants(),
  });
  const folderPicker = createMultiSelect($("#grant-folders"), {
    label: "folders and projects",
    placeholder: "Search folders, or type one…",
    noneText: "The whole space",
    custom: (text) => parseFolder(text),
    onChange: () => previewGrants(),
  });
  const boxesOf = (name) => [...grantForm.querySelectorAll(`input[name="${name}"]`)];
  const checkedOf = (name) => boxesOf(name).filter((box) => box.checked).map((box) => box.value);

  /** The form as grants, or the reason it is not yet. */
  const currentPlan = () => {
    try {
      return {
        plan: grantPlan({
          principals: whoPicker.values(),
          spaces: checkedOf("grant-space"),
          folders: folderPicker.values(),
          kinds: checkedOf("grant-kind"),
          permission: $("#grant-permission").value,
          expires: $("#grant-expires").value,
        }),
      };
    } catch (error) {
      return { error: error.message };
    }
  };

  function previewGrants() {
    const { plan } = currentPlan();
    grantPreviewLine.textContent = plan ? grantPreview(plan) : "";
    grantSubmit.textContent = plan && plan.count > 1 ? `Create ${plan.count} grants` : "Grant";
    grantSubmit.disabled = Boolean(plan && plan.count > MAX_BATCH);
  }

  const fillPrincipals = () => {
    whoPicker.setOptions(principalOptions(members, agents, me && me.member && me.member.user_id));
  };

  const loadFolders = async () => {
    // Bounded reads the wiki already makes, each filtered to what this
    // member may see: no folder name reaches the list that they could not.
    const [tree, pages, projects] = await Promise.all([
      api("/v1/coding-agents/tree").catch(() => null),
      api("/v1/pages?limit=200").catch(() => null),
      api("/v1/sources/projects").catch(() => null),
    ]);
    folderPicker.setOptions(folderOptions({ tree, pages, projects }, projectSegment));
  };

  // Everything is exclusive in *What*: ticking it clears the kinds, ticking
  // a kind clears it.
  grantForm.addEventListener("change", (event) => {
    const box = event.target;
    if (box && box.name === "grant-kind" && box.checked) {
      for (const other of boxesOf("grant-kind")) {
        if (other !== box && (box.value === "*" || other.value === "*")) other.checked = false;
      }
    }
    previewGrants();
  });

  const paintAgentSelection = () => {
    agentSelection = keepSelected(agentSelection, agents);
    const n = agentSelection.size;
    agentSelectedLine.textContent = selectionText(n, "agent");
    agentDisableSelected.disabled = n === 0;
    agentEnableSelected.disabled = n === 0;
    agentSelectAll.checked = n > 0 && n === agents.length;
    agentSelectAll.indeterminate = n > 0 && n < agents.length;
  };

  const paintGrantSelection = () => {
    const n = [...grantSelection].filter((id) => grantsShown.some((g) => g.id === id)).length;
    grantSelectedLine.textContent = selectionText(n, "grant");
    grantRevokeSelected.disabled = n === 0;
    grantRevokeSelected.textContent = n ? `Revoke ${n} selected` : "Revoke selected";
    grantSelectAll.checked = n > 0 && n === grantsShown.length;
    grantSelectAll.indeterminate = n > 0 && n < grantsShown.length;
  };

  const fillFilter = (select, all, choices, current) => {
    select.replaceChildren();
    for (const { value, label } of [{ value: "", label: all }, ...choices]) {
      const option = document.createElement("option");
      option.value = value;
      option.textContent = label;
      select.appendChild(option);
    }
    select.value = choices.some((c) => c.value === current) ? current : "";
    return select.value;
  };

  const paintGrants = () => {
    const choices = grantFilterOptions(grants, members, agents);
    grantFilter.who = fillFilter(grantFilterWho, "Anyone", choices.who, grantFilter.who);
    grantFilter.where = fillFilter(grantFilterWhere, "Anywhere", choices.where, grantFilter.where);
    grantFilter.what = fillFilter(grantFilterWhat, "Anything", choices.what, grantFilter.what);
    grantsShown = filterGrants(grants, grantFilter);
    grantSelection = keepSelected(grantSelection, grantsShown);
    renderGrantRows(
      grantRows,
      grantsShown,
      members,
      agents,
      document,
      grantSelection,
      grants.length ? "No grants match the filter." : undefined,
    );
    paintGrantSelection();
  };

  const loadKeys = async () => {
    if (!openAgent) return;
    try {
      renderKeyRows(keyRows, keyViews(await send(agentKeysRequest(openAgent.id))));
    } catch (error) {
      toast(accessErrorMessage(error, "load the keys"), "bad");
    }
  };

  const loadAgents = async () => {
    try {
      agents = agentViews(await send(agentListRequest()));
      agentSelection = keepSelected(agentSelection, agents);
      renderAgentRows(agentRows, agents, document, agentSelection);
      paintAgentSelection();
      notice(agentsStatus, "");
    } catch (error) {
      notice(agentsStatus, accessErrorMessage(error, "load agent accounts"), "warn");
    }
  };

  const loadGrants = async () => {
    try {
      grants = grantViews(await send(grantListRequest()));
      paintGrants();
      notice(accessStatus, "");
    } catch (error) {
      notice(accessStatus, accessErrorMessage(error, "load access"), "warn");
    }
  };

  const loadAudit = async (more) => {
    if (!more) {
      auditRows.replaceChildren();
      auditCursor = "";
    }
    try {
      const page = await send(auditRequest(auditCursor));
      renderAuditRows(auditRows, page.entries);
      auditCursor = page.next || "";
      auditMore.hidden = !auditCursor;
    } catch (error) {
      toast(accessErrorMessage(error, "load the audit log"), "bad");
    }
  };

  agentForm.addEventListener("submit", async (event) => {
    event.preventDefault();
    let request;
    try {
      request = agentCreateRequest($("#agent-name").value, $("#agent-description").value);
    } catch {
      fieldError("agentName", "Give it a name.", $("#agent-name"));
      return;
    }
    try {
      await send(request);
      agentForm.reset();
      toast("Agent account created. It has no access until you grant some.", "ok");
      await loadAgents();
      fillPrincipals();
      paintGrants();
    } catch (error) {
      toast(accessErrorMessage(error, "create the agent account"), "bad");
    }
  });

  agentRows.addEventListener("change", (event) => {
    const box = event.target.closest("[data-agent-select]");
    if (!box) return;
    const id = box.getAttribute("data-agent-select");
    if (box.checked) agentSelection.add(id);
    else agentSelection.delete(id);
    paintAgentSelection();
  });

  agentSelectAll.addEventListener("change", () => {
    agentSelection = new Set(agentSelectAll.checked ? agents.map((a) => a.id) : []);
    renderAgentRows(agentRows, agents, document, agentSelection);
    paintAgentSelection();
  });

  const bulkAgents = async (disabled) => {
    const ids = [...agentSelection];
    if (!ids.length) return;
    try {
      if (ids.length > MAX_BATCH) throw new Error(`choose at most ${MAX_BATCH}`);
      if (disabled) {
        const ok = await confirmAction({
          title: `Disable ${selectionText(ids.length, "agent account").replace(" selected", "")}?`,
          body: "Their keys stop working at once. Enabling them again brings back every key that is not revoked or expired.",
          confirm: "Disable",
        });
        if (!ok) return;
      }
      const done = await send(agentBatchRequest(ids, disabled));
      const n = Number(done && done.changed) || 0;
      toast(
        disabled
          ? `Disabled ${n}: their keys stop working now.`
          : `Enabled ${n}.`,
        "ok",
      );
      agentSelection = new Set();
      await loadAgents();
    } catch (error) {
      toast(accessErrorMessage(error, disabled ? "disable those agent accounts" : "enable those agent accounts"), "bad");
    }
  };
  agentDisableSelected.addEventListener("click", () => bulkAgents(true));
  agentEnableSelected.addEventListener("click", () => bulkAgents(false));

  agentRows.addEventListener("click", async (event) => {
    const keys = event.target.closest("[data-agent-keys]");
    const toggle = event.target.closest("[data-agent-toggle]");
    const remove = event.target.closest("[data-agent-delete]");
    try {
      if (keys) {
        openAgent = { id: keys.dataset.agentKeys, name: keys.dataset.name };
        keysTitle.textContent = `Keys of ${openAgent.name}`;
        keysBox.hidden = false;
        await loadKeys();
      } else if (toggle) {
        const disabled = toggle.dataset.disabled !== "1";
        await send(agentUpdateRequest(toggle.dataset.agentToggle, { disabled }));
        toast(disabled ? "Disabled: its keys stop working now." : "Enabled.", "ok");
        await loadAgents();
      } else if (remove) {
        const ok = await confirmAction({
          title: `Delete ${remove.dataset.name}?`,
          body: "Its keys stop working and every grant to it is removed. This cannot be undone.",
          confirm: "Delete",
        });
        if (!ok) return;
        await send(agentDeleteRequest(remove.dataset.agentDelete));
        if (openAgent && openAgent.id === remove.dataset.agentDelete) keysBox.hidden = true;
        toast(`Deleted ${remove.dataset.name}.`, "ok");
        await loadAgents();
        await loadGrants();
      }
    } catch (error) {
      toast(accessErrorMessage(error, "change the agent account"), "bad");
    }
  });

  keyForm.addEventListener("submit", async (event) => {
    event.preventDefault();
    if (!openAgent) return;
    try {
      showKey(await send(agentKeyMintRequest(openAgent.id, $("#agent-key-name").value, $("#agent-key-days").value)));
      keyForm.reset();
      await loadKeys();
    } catch (error) {
      toast(accessErrorMessage(error, "create the key"), "bad");
    }
  });

  keyRows.addEventListener("click", async (event) => {
    const rotate = event.target.closest("[data-key-rotate]");
    const revoke = event.target.closest("[data-key-revoke]");
    if (!openAgent || (!rotate && !revoke)) return;
    try {
      if (rotate) {
        showKey(await send(agentKeyRotateRequest(openAgent.id, rotate.dataset.keyRotate)));
      } else {
        await send(agentKeyRevokeRequest(openAgent.id, revoke.dataset.keyRevoke));
        toast("Revoked. Anything using it stops now.", "ok");
      }
      await loadKeys();
    } catch (error) {
      toast(accessErrorMessage(error, "change the key"), "bad");
    }
  });

  $("#copy-agent-key").addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(keyValue.textContent);
      toast("Copied.", "ok");
    } catch {
      toast("Copying was blocked. Select the key and copy it by hand.", "warn");
    }
  });
  $("#dismiss-agent-key").addEventListener("click", forget);
  window.addEventListener("pagehide", forget);

  grantForm.addEventListener("submit", async (event) => {
    event.preventDefault();
    for (const id of ["grantWho", "grantSpaces", "grantKinds", "grantExpires"]) fieldError(id, "");
    const { plan, error } = currentPlan();
    if (!plan) {
      const where = /who/.test(error)
        ? ["grantWho", whoPicker.search]
        : /space/.test(error)
          ? ["grantSpaces", boxesOf("grant-space")[0]]
          : /covers/.test(error)
            ? ["grantKinds", boxesOf("grant-kind")[0]]
            : /expiry/.test(error)
              ? ["grantExpires", $("#grant-expires")]
              : ["grantWho", whoPicker.search];
      fieldError(where[0], `Please ${error}.`.replace("Please the", "The"));
      if (where[1]) where[1].focus();
      return;
    }
    let request;
    try {
      request = grantBatchRequest(plan.grants);
    } catch (reason) {
      toast(reason.message, "warn");
      return;
    }
    if (needsConfirm(plan.count)) {
      const ok = await confirmAction({
        title: `Create ${plan.count} grants?`,
        body: grantPreview(plan),
        confirm: `Create ${plan.count}`,
      });
      if (!ok) return;
    }
    try {
      const done = await send(request);
      const n = Number(done && done.created) || plan.count;
      toast(n === 1 ? `Granted to ${principalLabel(plan.grants[0].principal, members, agents)}.` : `Created ${n} grants.`, "ok");
      whoPicker.clear();
      folderPicker.clear();
      grantForm.reset();
      previewGrants();
      await loadGrants();
    } catch (reason) {
      toast(accessErrorMessage(reason, "grant that"), "bad");
    }
  });

  for (const [select, key] of [
    [grantFilterWho, "who"],
    [grantFilterWhere, "where"],
    [grantFilterWhat, "what"],
  ]) {
    select.addEventListener("change", () => {
      grantFilter[key] = select.value;
      paintGrants();
    });
  }

  grantRows.addEventListener("change", (event) => {
    const box = event.target.closest("[data-grant-select]");
    if (!box) return;
    const id = box.getAttribute("data-grant-select");
    if (box.checked) grantSelection.add(id);
    else grantSelection.delete(id);
    paintGrantSelection();
  });

  grantSelectAll.addEventListener("change", () => {
    // "All" is all the filter shows: a hidden row is never revoked by it.
    grantSelection = new Set(grantSelectAll.checked ? grantsShown.map((g) => g.id) : []);
    renderGrantRows(grantRows, grantsShown, members, agents, document, grantSelection);
    paintGrantSelection();
  });

  grantRevokeSelected.addEventListener("click", async () => {
    const ids = [...grantSelection].filter((id) => grantsShown.some((g) => g.id === id));
    if (!ids.length) return;
    let request;
    try {
      request = grantRevokeBatchRequest(ids);
    } catch (reason) {
      toast(reason.message, "warn");
      return;
    }
    const ok = await confirmAction({
      title: ids.length === 1 ? "Revoke this grant?" : `Revoke ${ids.length} grants?`,
      body: "Whoever they name loses that access at once. Either all of them are revoked or none is.",
      confirm: "Revoke",
    });
    if (!ok) return;
    try {
      const done = await send(request);
      toast(`Revoked ${Number(done && done.revoked) || ids.length}.`, "ok");
      grantSelection = new Set();
      await loadGrants();
    } catch (reason) {
      toast(accessErrorMessage(reason, "revoke those grants"), "bad");
    }
  });

  grantRows.addEventListener("click", async (event) => {
    const remove = event.target.closest("[data-grant-delete]");
    if (!remove) return;
    try {
      await send(grantDeleteRequest(remove.dataset.grantDelete));
      toast("Removed.", "ok");
      await loadGrants();
    } catch (error) {
      toast(accessErrorMessage(error, "remove that grant"), "bad");
    }
  });

  auditMore.addEventListener("click", () => loadAudit(true));

  try {
    me = await api("/v1/workspaces/me");
    members = await send(membersRequest());
  } catch {
    members = [];
  }
  await loadAgents();
  fillPrincipals();
  previewGrants();
  await loadGrants();
  loadFolders();
  const admin = canManage(me);
  // The audit log is for owners and admins: the Access tab shows it only
  // when the section is marked allowed (`showSettingsTab`).
  auditSection.dataset.allowed = admin ? "true" : "false";
  showSettingsTab(location.hash);
  if (admin) await loadAudit(false);
}

/* --- wiki ----------------------------------------------------------- */

// The controller itself lives in `wiki.js`, imported above; boot passes no
// arguments, so the registry entry closes over this module's `api` wrapper.
pages.wiki = () => wikiPage(api);
pages.dreaming = () => dreamingPage(api);

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
  signOut,
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