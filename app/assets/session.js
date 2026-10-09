// Who is signed in, and what each page shows for it.
//
// Pure: no `fetch`, no `document`. The pages hand in the `api` wrapper and
// plain element handles, so `node --test` exercises the same functions the
// pages run — the request, the three-way decision (signed in, signed out,
// could not tell), and what becomes visible for each.
//
// Nothing here ever holds a credential. The session is the HttpOnly
// `__Host-lb_session` cookie the browser attaches by itself; `/me` answers
// who it names, and `/signout` asks the API to drop it.

/** Who the session names: the workspace, the member, and `is_owner`. */
export const ME_PATH = "/v1/workspaces/me";

/** Ends the session: `204` and a `Set-Cookie` with `Max-Age=0`. */
export const SIGNOUT_PATH = "/v1/workspaces/signout";

/** Where a coding agent reaches the brain over MCP, on any origin. */
export const MCP_PATH = "/v1/pages/mcp";

/**
 * How long the check may take before the page stops waiting and says so. The
 * sign-in form is hidden while the check runs, so a request that never
 * settles must not leave the visitor with nothing to use.
 */
export const CHECK_TIMEOUT_MS = 10000;

/**
 * Asks the API who is signed in, and answers one of three states:
 *
 * - `{ kind: "signed-in", me }` on a 200 — `me` is the `/me` body;
 * - `{ kind: "signed-out" }` on a 401 — no session, or one that no longer
 *   verifies; the sign-in form is the right answer;
 * - `{ kind: "error", status, message }` on anything else, including a
 *   network failure and a check that took too long. The page cannot tell
 *   whether the visitor is signed in, so it says so and still offers the
 *   form.
 *
 * Never throws: every outcome is one of the three.
 */
export async function checkSession(api, { timeoutMs = CHECK_TIMEOUT_MS } = {}) {
  let timer;
  const timeout = new Promise((resolve) => {
    timer = setTimeout(
      () => resolve({ kind: "error", status: 0, message: "the session check timed out" }),
      timeoutMs,
    );
  });
  const check = (async () => {
    try {
      const me = await api(ME_PATH);
      if (!me || typeof me !== "object" || !me.workspace) {
        return { kind: "error", status: 200, message: "the API answered without a workspace" };
      }
      return { kind: "signed-in", me };
    } catch (error) {
      const status = error && typeof error.status === "number" ? error.status : 0;
      if (status === 401) return { kind: "signed-out" };
      return {
        kind: "error",
        status,
        message: (error && error.message) || "the session check failed",
      };
    }
  })();
  try {
    return await Promise.race([check, timeout]);
  } finally {
    clearTimeout(timer);
  }
}

/**
 * What the signed-in home and the header show, from a `/me` body and the
 * origin the API is on. Every field is plain text for `textContent`.
 */
export function sessionView(me, origin) {
  const workspace = (me && me.workspace) || {};
  const member = (me && me.member) || {};
  const name = typeof member.name === "string" ? member.name.trim() : "";
  const base = String(origin || "").replace(/\/+$/, "");
  return {
    who: name || member.user_id || "a member",
    workspaceName: workspace.name || "Your workspace",
    workspaceId: workspace.id || "",
    role: me && me.is_owner ? "Owner" : "Member",
    mcpUrl: `${base}${MCP_PATH}`,
    apiBase: base,
  };
}

/** The sign-out request, built in one place so the test pins it. */
export function signOutRequest() {
  return { method: "POST", path: SIGNOUT_PATH };
}

/** The notice text for a check that could not decide. */
export function errorMessage(state) {
  const status = state && state.status ? ` (HTTP ${state.status})` : "";
  return (
    `We could not check whether you are signed in${status}. ` +
    "You can still sign in below, or reload to try again."
  );
}

/** The text slots a page fills from {@link sessionView}. */
export const SLOTS = Object.freeze([
  "who",
  "workspaceName",
  "workspaceId",
  "role",
  "mcpUrl",
  "apiBase",
]);

/**
 * Shows the right part of the sign-in page for a state.
 *
 * `els` holds the page's handles: `checking`, `signin`, `home`, `error`, and
 * the text slots named in {@link SLOTS} — each one element or a list. Each
 * needs only `hidden` and `textContent`, so a test passes plain objects. `state.kind` is `"checking"` before the check settles.
 */
export function applySignInPage(els, state, origin) {
  const kind = state ? state.kind : "checking";
  const set = (el, hidden) => {
    if (el) el.hidden = hidden;
  };
  set(els.checking, kind !== "checking");
  set(els.home, kind !== "signed-in");
  // The form stays hidden only while the check runs and once it has found a
  // session; signed out and "could not tell" both leave it usable.
  set(els.signin, kind === "checking" || kind === "signed-in");
  set(els.error, kind !== "error");
  if (els.error) els.error.textContent = kind === "error" ? errorMessage(state) : "";
  if (kind === "signed-in") {
    const view = sessionView(state.me, origin);
    for (const key of SLOTS) {
      // A slot may appear more than once on a page (the workspace name is
      // the heading and a fact), so each key takes one element or a list.
      for (const el of [].concat(els[key] || [])) el.textContent = view[key];
    }
    return view;
  }
  return null;
}

/**
 * The header on the other pages: "Signed in as …" and Sign out when there is
 * a session, the plain "Sign in" link otherwise (and while checking, and
 * when the check could not tell — a link is never wrong).
 *
 * `els` holds `bar` (the signed-in group), `who`, and `signin` (the link).
 */
export function applyHeader(els, state) {
  const signedIn = Boolean(state && state.kind === "signed-in");
  if (els.bar) els.bar.hidden = !signedIn;
  if (els.signin) els.signin.hidden = signedIn;
  if (els.who) els.who.textContent = signedIn ? sessionView(state.me, "").who : "";
  return signedIn;
}
