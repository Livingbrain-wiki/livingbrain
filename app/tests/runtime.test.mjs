// The browser glue, imported as the pages import it.
//
// `app/assets/app.js` guards its DOM boot, so Node can import the very file
// the pages load. These tests therefore exercise the real `api` wrapper and the
// real `LB.setting` choke point — including the request they put on the wire —
// rather than a re-implementation.

import test from "node:test";
import assert from "node:assert/strict";

import { api, ApiError, esc, LB } from "../assets/app.js";

/** Replaces global fetch and records what was asked for. */
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

const json = (body, status = 200) =>
  new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });

test("LB exposes the settings choke point and the helpers", () => {
  assert.equal(typeof LB.setting, "function");
  assert.equal(typeof LB.api, "function");
  assert.equal(typeof LB.$, "function");
  assert.equal(typeof LB.$$, "function");
  assert.equal(typeof LB.esc, "function");
  assert.ok(Object.isFrozen(LB), "LB must not be mutable by a page");
});

test("api sends the session cookie, never a token", async () => {
  await withFetch(
    () => json({ ok: true }),
    async (calls) => {
      await api("/v1/models/");
      assert.equal(calls[0].init.credentials, "same-origin");
      const headers = calls[0].init.headers;
      // No Authorization header of any kind: the session is HttpOnly.
      for (const name of Object.keys(headers)) {
        assert.ok(!/authorization/i.test(name), `sent ${name}`);
      }
      assert.equal(calls[0].init.body, undefined);
    },
  );
});

test("api parses the problem-details shape the API answers with", async () => {
  const shapes = [
    { error: "authorization_pending" },
    { message: "not allowed" },
    { detail: "the model endpoint did not respond" },
    { title: "Admin or owner access required" },
  ];
  for (const body of shapes) {
    await withFetch(
      () => json(body, 403),
      async () => {
        await assert.rejects(
          () => api("/v1/models/main"),
          (error) => {
            assert.ok(error instanceof ApiError);
            assert.equal(error.status, 403);
            assert.deepEqual(error.body, body);
            return true;
          },
        );
      },
    );
  }
});

test("api turns a network failure into an ApiError, not an unhandled throw", async () => {
  const original = globalThis.fetch;
  globalThis.fetch = async () => {
    throw new TypeError("failed to fetch");
  };
  try {
    await assert.rejects(() => api("/v1/models/"), (error) => {
      assert.ok(error instanceof ApiError);
      assert.equal(error.status, 0);
      return true;
    });
  } finally {
    globalThis.fetch = original;
  }
});

test("api survives a non-JSON body", async () => {
  await withFetch(
    () => new Response("<html>gateway</html>", { status: 502 }),
    async () => {
      await assert.rejects(() => api("/v1/models/"), (error) => {
        assert.ok(error instanceof ApiError);
        assert.equal(error.status, 502);
        return true;
      });
    },
  );
});

test("LB.setting is the only write path, and it sends the audit triple", async () => {
  await withFetch(
    () => json({ role: "main" }, 200),
    async (calls) => {
      const outcome = await LB.setting("models", "main", {
        provider: "deepseek",
        api_key: "sk-test",
        model: "deepseek-chat",
      });
      assert.equal(outcome.ok, true);
      assert.equal(outcome.sent, true);

      const call = calls[0];
      assert.equal(call.url, "/v1/models/main");
      assert.equal(call.init.method, "PUT");
      assert.equal(call.init.credentials, "same-origin");

      // What the server receives on the wire.
      const sent = JSON.parse(call.init.body);
      assert.equal(sent.provider, "deepseek");
      assert.equal(sent.api_key, "sk-test");
      // And the triple that makes the write auditable.
      assert.equal(sent.section, "models");
      assert.equal(sent.key, "main");
      assert.deepEqual(sent.value, {
        provider: "deepseek",
        api_key: "sk-test",
        model: "deepseek-chat",
      });
    },
  );
});

test("LB.setting sends nothing for a section with no route", async () => {
  await withFetch(
    () => json({}),
    async (calls) => {
      const outcome = await LB.setting("skills", "shared-library", true);
      assert.equal(outcome.ok, false);
      assert.equal(outcome.sent, false);
      assert.match(outcome.message, /no server route yet/);
      assert.equal(calls.length, 0, "an unwired setting must not hit the API");
    },
  );
});

test("LB.setting surfaces the server's refusal", async () => {
  await withFetch(
    () => json({ error: "forbidden" }, 403),
    async (calls) => {
      const outcome = await LB.setting("models", "triage", { provider: "x" });
      assert.equal(outcome.ok, false);
      assert.equal(outcome.sent, true);
      assert.equal(calls.length, 1);
      assert.match(outcome.message, /forbidden/);
    },
  );
});

test("LB.setting refuses an unknown section rather than posting it", async () => {
  await withFetch(
    () => json({}),
    async (calls) => {
      await assert.rejects(() => LB.setting("billing", "plan", "pro"), /unknown settings section/);
      assert.equal(calls.length, 0);
    },
  );
});

test("esc escapes every character that could close an attribute", () => {
  assert.equal(esc(`<script>"x"&'`), "&lt;script&gt;&quot;x&quot;&amp;&#39;");
  assert.equal(esc(null), "");
  assert.equal(esc(0), "0");
});