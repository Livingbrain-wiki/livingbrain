// Signed in versus signed out.
//
// `session.js` is what the sign-in page and the other pages' headers run: the
// `/me` check, the three-way decision, and what becomes visible for each. The
// tests drive it with the real `api` wrapper from `app.js` over a stubbed
// `fetch`, and hand it plain objects for elements, so what is asserted is the
// code the pages run.

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

import { api, signOut } from "../assets/app.js";
import {
  checkSession,
  sessionView,
  applySignInPage,
  applyHeader,
  signOutRequest,
  errorMessage,
  ME_PATH,
  SIGNOUT_PATH,
  SLOTS,
} from "../assets/session.js";

const ME = {
  workspace: { id: "ws_01JTEST", name: "Acme", owner_id: "usr_01" },
  member: { user_id: "usr_01", name: "ada", timezone: null, is_admin: false },
  is_owner: true,
};

function withFetch(handler, run) {
  const calls = [];
  const original = globalThis.fetch;
  globalThis.fetch = async (url, init) => {
    calls.push({ url: String(url), init });
    return handler(String(url), init);
  };
  return Promise.resolve(run(calls)).finally(() => {
    globalThis.fetch = original;
  });
}

const answer = (body, status = 200) =>
  new Response(body === null ? null : JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });

/** Element stand-ins: only `hidden` and `textContent` are ever touched. */
function page() {
  const el = () => ({ hidden: false, textContent: "" });
  const els = { checking: el(), signin: el(), home: el(), error: el() };
  for (const key of SLOTS) els[key] = [el(), el()];
  return els;
}

const visible = (els) =>
  ["checking", "signin", "home", "error"].filter((key) => !els[key].hidden);

test("the check asks /me with the cookie and no token", async () => {
  await withFetch(
    () => answer(ME),
    async (calls) => {
      const state = await checkSession(api);
      assert.equal(state.kind, "signed-in");
      assert.equal(calls.length, 1);
      assert.equal(calls[0].url, ME_PATH);
      assert.equal(calls[0].init.method, "GET");
      assert.equal(calls[0].init.credentials, "same-origin");
      for (const name of Object.keys(calls[0].init.headers)) {
        assert.ok(!/authorization/i.test(name), `sent ${name}`);
      }
    },
  );
});

test("a 200 is signed in, a 401 signed out, anything else an error", async () => {
  const cases = [
    [() => answer(ME), "signed-in"],
    [() => answer({ title: "Sign in with Slack" }, 401), "signed-out"],
    [() => answer({ title: "boom" }, 500), "error"],
    [() => answer({ title: "nope" }, 403), "error"],
    [() => answer({ unexpected: true }), "error"],
    [
      () => {
        throw new TypeError("offline");
      },
      "error",
    ],
  ];
  for (const [handler, kind] of cases) {
    await withFetch(handler, async () => {
      assert.equal((await checkSession(api)).kind, kind);
    });
  }
});

test("a check that never settles becomes an error, not an endless spinner", async () => {
  const state = await checkSession(() => new Promise(() => {}), { timeoutMs: 5 });
  assert.equal(state.kind, "error");
});

test("while checking, neither the form nor the home is shown", () => {
  const els = page();
  applySignInPage(els, { kind: "checking" }, "https://brain.example");
  assert.deepEqual(visible(els), ["checking"]);
});

test("signed in shows the home with the workspace and the MCP endpoint", () => {
  const els = page();
  const view = applySignInPage(els, { kind: "signed-in", me: ME }, "https://brain.example");
  assert.deepEqual(visible(els), ["home"]);
  for (const el of els.workspaceName) assert.equal(el.textContent, "Acme");
  for (const el of els.workspaceId) assert.equal(el.textContent, "ws_01JTEST");
  for (const el of els.who) assert.equal(el.textContent, "ada");
  for (const el of els.role) assert.equal(el.textContent, "Owner");
  for (const el of els.mcpUrl) {
    assert.equal(el.textContent, "https://brain.example/v1/pages/mcp");
  }
  assert.equal(view.apiBase, "https://brain.example");
});

test("signed out shows today's sign-in form and nothing else", () => {
  const els = page();
  applySignInPage(els, { kind: "signed-in", me: ME }, "https://brain.example");
  applySignInPage(els, { kind: "signed-out" }, "https://brain.example");
  assert.deepEqual(visible(els), ["signin"]);
  assert.equal(els.error.textContent, "");
});

test("an error shows a plain notice and keeps the form usable", () => {
  const els = page();
  applySignInPage(els, { kind: "error", status: 502 }, "https://brain.example");
  assert.deepEqual(visible(els), ["signin", "error"]);
  assert.match(els.error.textContent, /could not check/);
  assert.match(els.error.textContent, /HTTP 502/);
  assert.doesNotMatch(errorMessage({ kind: "error", status: 0 }), /HTTP/);
});

test("the view falls back to the member id, and a member is not an owner", () => {
  const view = sessionView(
    { ...ME, member: { user_id: "U0ALICE", name: "  " }, is_owner: false },
    "https://brain.example/",
  );
  assert.equal(view.who, "U0ALICE");
  assert.equal(view.role, "Member");
  assert.equal(view.mcpUrl, "https://brain.example/v1/pages/mcp");
});

test("the header shows who is signed in, or the sign-in link", () => {
  const el = () => ({ hidden: false, textContent: "" });
  const els = { bar: el(), who: el(), signin: el() };
  applyHeader(els, { kind: "signed-in", me: ME });
  assert.equal(els.bar.hidden, false);
  assert.equal(els.signin.hidden, true);
  assert.equal(els.who.textContent, "ada");
  for (const state of [{ kind: "signed-out" }, { kind: "error", status: 500 }]) {
    applyHeader(els, state);
    assert.equal(els.bar.hidden, true, state.kind);
    assert.equal(els.signin.hidden, false, state.kind);
  }
});

test("sign out is one POST to /signout, with the cookie and no body", async () => {
  assert.deepEqual(signOutRequest(), { method: "POST", path: SIGNOUT_PATH });
  await withFetch(
    () => new Response(null, { status: 204 }),
    async (calls) => {
      assert.equal(await signOut(), true);
      assert.equal(calls.length, 1);
      assert.equal(calls[0].url, "/v1/workspaces/signout");
      assert.equal(calls[0].init.method, "POST");
      assert.equal(calls[0].init.credentials, "same-origin");
      assert.equal(calls[0].init.body, undefined);
    },
  );
});

test("a refused sign out throws, so the page can say so", async () => {
  await withFetch(
    () => answer({ title: "This request came from another site" }, 403),
    async () => {
      await assert.rejects(signOut(), (error) => error.status === 403);
    },
  );
});

test("the sign-in page hides both views until the check settles", () => {
  const html = readFileSync(new URL("../index.html", import.meta.url), "utf8");
  const tag = (id) => html.match(new RegExp(`<[^>]*id="${id}"[^>]*>`))[0];
  assert.match(tag("signin-view"), /\shidden[\s>]/);
  assert.match(tag("home-view"), /\shidden[\s>]/);
  assert.doesNotMatch(tag("session-checking"), /\shidden[\s>]/);
  assert.match(html, /Checking your session…/);
});
