// API tokens — the pure half, next to `settings.js`.
//
// A token value is returned by exactly one call, `POST /v1/tokens`. This module
// is built around that: `tokenView` copies named fields out of an entry and
// nothing else, so no list response can reach the page with a value in it; and
// `createdTokenView` is the only function that reads `response.token`.
// Requests are described here and sent by `app.js`, as `settingRequest` is
// described in `settings.js` and sent by `LB.setting`.

/** Every token route hangs off this path. */
export const TOKENS_PATH = "/v1/tokens";

/* ------------------------------------------------- the sign-in return target */

/**
 * The query parameter the device-approval hook appends when it bounces an
 * anonymous visitor to the sign-in page — `/index.html?return_to=…`, built by
 * `sign_in_location` in `crates/livingbrain-tokens/src/device.rs`. The app
 * navigates back to the value after a successful sign-in.
 */
export const RETURN_TO_PARAM = "return_to";

/**
 * The `return_to` value, but only when it names a place on this origin.
 *
 * The value arrives in the page URL, where anyone can write anything, and the
 * app navigates to it after sign-in — so it is accepted only as a non-empty
 * *relative path*: it must start with a single `/`. `//host` is a
 * protocol-relative URL, `/\host` is parsed the same way, and both would leave
 * the origin; a scheme is impossible once the first character must be `/`;
 * control characters are refused anywhere, because they can smuggle a newline
 * into a URL. Everything else — `javascript:` URIs, absolute URLs, bare
 * relative paths, the empty string — answers null, and the caller ignores it.
 */
export function safeReturnTo(value) {
  const path = value == null ? "" : String(value);
  if (path.charCodeAt(0) !== 0x2f /* "/" */) return null;
  const second = path.charCodeAt(1);
  if (second === 0x2f || second === 0x5c /* "/" or "\" */) return null;
  for (let i = 0; i < path.length; i += 1) {
    const code = path.charCodeAt(i);
    if (code < 0x20 || code === 0x7f) return null;
  }
  return path;
}

/**
 * Reads the optional scopes out of the comma-separated text field: trimmed,
 * de-duplicated, in the order typed. `""` is not an empty list of scopes to
 * send — it is *no* scopes, so `tokenCreateRequest` leaves the key out entirely.
 */
export function parseScopes(input) {
  const text = input == null ? "" : String(input);
  return [...new Set(text.split(",").map((part) => part.trim()).filter(Boolean))];
}

/** The list read: `GET /v1/tokens`. */
export function tokenListRequest() {
  return { method: "GET", path: TOKENS_PATH };
}

/**
 * The create request. Throws on an empty name: a token nobody can recognise in
 * the list is one that cannot be revoked sensibly later.
 */
export function tokenCreateRequest(name, scopeText) {
  const label = String(name == null ? "" : name).trim();
  if (!label) throw new Error("a token needs a name");
  const scopes = parseScopes(scopeText);
  return {
    method: "POST",
    path: TOKENS_PATH,
    body: scopes.length ? { name: label, scopes } : { name: label },
  };
}

/** The revoke request. The prefix is encoded, so it cannot walk out of the path. */
export function tokenRevokeRequest(prefix) {
  const id = String(prefix == null ? "" : prefix).trim();
  if (!id) throw new Error("a token needs a prefix to revoke");
  return { method: "DELETE", path: `${TOKENS_PATH}/${encodeURIComponent(id)}` };
}

/** The created date, or an em dash when the server sent nothing parseable. */
export function formatCreated(value) {
  const raw = value == null ? "" : String(value);
  if (!raw) return "—";
  const at = new Date(raw);
  return Number.isNaN(at.getTime()) ? "—" : at.toISOString().slice(0, 10);
}

/**
 * One list entry, as the page shows it. Deliberately not a spread of the entry:
 * only four fields are read, so the view model cannot carry a secret even if the
 * response did. No scopes means the member's own full read access, and
 * "Full access" says that where an empty cell would read as "nothing".
 */
export function tokenView(entry) {
  const scopes = Array.isArray(entry && entry.scopes)
    ? entry.scopes.map(String).filter(Boolean)
    : [];
  return {
    prefix: String((entry && entry.prefix) || ""),
    name: String((entry && entry.name) || ""),
    scopes,
    scopeSummary: scopes.length ? scopes.join(", ") : "Full access",
    created: formatCreated(entry && entry.created_at),
  };
}

/** The list, as view models. A missing or non-array `tokens` is an empty list. */
export function tokenViews(response) {
  const list = Array.isArray(response && response.tokens) ? response.tokens : [];
  return list.map(tokenView);
}

/**
 * The create response — the *only* place in the app where a token value is read.
 * Read as a list row instead, `tokenView` has no way to reach it.
 */
export function createdTokenView(response) {
  return { ...tokenView(response), token: String((response && response.token) || "") };
}

/** What the notice says when a token call fails. */
export function tokenErrorMessage(error, what) {
  if (error && error.status === 401) {
    return "You are signed out — sign in again to manage API tokens.";
  }
  return `Could not ${what}: ${(error && error.message) || String(error)}`;
}

/**
 * Paints the token rows. Elements and text nodes only, as everywhere else in
 * `app/`. Each revoke button carries the prefix as `data-prefix`, which is how
 * `app.js` finds the row without a handler per row.
 */
export function renderTokenRows(container, views, doc = typeof document === "undefined" ? null : document) {
  if (!container || !doc) return container;
  const list = Array.isArray(views) ? views : [];
  container.replaceChildren();

  const row = list.length ? list : [null];
  for (const view of row) {
    const tr = doc.createElement("tr");
    for (const value of view
      ? [view.name, view.prefix, view.scopeSummary, view.created]
      : ["No API tokens yet."]) {
      const td = doc.createElement("td");
      td.textContent = value;
      tr.appendChild(td);
    }
    if (view) {
      const actions = doc.createElement("td");
      const revoke = doc.createElement("button");
      revoke.type = "button";
      revoke.textContent = "Revoke";
      revoke.setAttribute("data-prefix", view.prefix);
      revoke.setAttribute("data-name", view.name);
      revoke.setAttribute("aria-label", `Revoke the token ${view.name}`);
      actions.appendChild(revoke);
      tr.appendChild(actions);
    } else {
      tr.firstChild.colSpan = 5;
    }
    container.appendChild(tr);
  }
  return container;
}

/**
 * Puts the freshly created token in the one place it is shown — `node` is the
 * `<code>` the copy affordance reads from; nothing else ever receives it.
 */
export function revealToken(node, view) {
  if (node) node.textContent = (view && view.token) || "";
  return node;
}

/** Takes the value back out of the DOM once it has been copied or dismissed. */
export function clearToken(node) {
  return revealToken(node, null);
}