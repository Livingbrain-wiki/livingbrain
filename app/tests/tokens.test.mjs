// The four properties that matter about API tokens: scopes are parsed as typed,
// no list render can ever show a value, the one value a create returns reaches
// the page only through the one-time reveal and leaves it again on dismissal,
// and nothing is stored. The sign-in return target is tested here too, because
// it lives in this module and must be a same-origin path and nothing else.

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

import {
  parseScopes,
  tokenCreateRequest,
  tokenRevokeRequest,
  tokenViews,
  createdTokenView,
  revealToken,
  clearToken,
  renderTokenRows,
  safeReturnTo,
} from "../assets/tokens.js";

/**
 * A placeholder token value, built from words so nothing in this file has the
 * shape of a real credential — the same reasoning as the BYOK fixture in
 * `settings.test.mjs`.
 */
const FIXTURE_VALUE = ["lb", "pat", "fixture", "notsecret"].join("_");

/**
 * Runs `renderTokenRows` against a recorder document, in the style of
 * `dom.test.mjs`: any method it does not offer here does not exist, so a switch
 * to `innerHTML` would throw rather than pass quietly.
 */
function renderRows(views) {
  const original = globalThis.document;
  const make = (tag) => ({
    tag,
    textContent: "",
    colSpan: 0,
    attrs: {},
    children: [],
    setAttribute(name, value) {
      this.attrs[name] = value;
    },
    appendChild(child) {
      this.children.push(child);
    },
  });
  const container = {
    children: [],
    replaceChildren() {
      this.children = [];
    },
    appendChild(child) {
      this.children.push(child);
    },
  };
  globalThis.document = {
    createElement: (tag) => make(tag),
    createTextNode: (value) => make("#text"),
  };
  try {
    renderTokenRows(container, views);
    return container.children;
  } finally {
    globalThis.document = original;
  }
}

/** Every element under the rendered rows, flattened. */
function flatten(node, out = []) {
  for (const child of node.children || []) {
    out.push(child);
    flatten(child, out);
  }
  return out;
}

/** Every string that reaches the DOM in a rendered table. */
function renderedText(views) {
  return renderRows(views)
    .flatMap((row) => flatten(row))
    .flatMap((node) => [node.textContent, ...Object.values(node.attrs || {})])
    .map(String);
}

/** A list response that (wrongly) carries a value on every entry. */
const leakyList = {
  tokens: [
    {
      prefix: "lb_a1b2",
      name: "laptop",
      scopes: null,
      created_at: "2026-03-04T09:30:00Z",
      token: FIXTURE_VALUE,
    },
    {
      prefix: "lb_c3d4",
      name: "reader",
      scopes: ["shared"],
      created_at: "2026-04-01T00:00:00Z",
      token: FIXTURE_VALUE,
    },
  ],
};

test("scopes are split, trimmed and de-duplicated, and empty means no key", () => {
  assert.deepEqual(parseScopes(" shared , channel , shared "), ["shared", "channel"]);
  for (const value of ["", "   ", ",", null]) {
    assert.deepEqual(parseScopes(value), [], JSON.stringify(value));
    // Not `[]` on the wire: no scopes reads as full access server-side.
    assert.deepEqual(tokenCreateRequest("laptop", value).body, { name: "laptop" });
  }
  assert.deepEqual(tokenCreateRequest("reader", "shared").body, {
    name: "reader",
    scopes: ["shared"],
  });
  // A token nobody can recognise in the list cannot be revoked sensibly later.
  assert.throws(() => tokenCreateRequest("  ", ""), /needs a name/);
  assert.throws(() => tokenRevokeRequest(""), /needs a prefix/);
});

test("the list view model cannot carry a token value, whatever the server sent", () => {
  const views = tokenViews(leakyList);
  assert.equal("token" in views[0], false);
  assert.deepEqual(Object.keys(views[0]).sort(), [
    "created",
    "name",
    "prefix",
    "scopeSummary",
    "scopes",
  ]);
  // `scopes: null` is the API's way of saying "the member's own access".
  assert.equal(views[0].scopeSummary, "Full access");
  assert.equal(views[1].scopeSummary, "shared");
});

test("the rendered list never shows a token value", () => {
  const strings = renderedText(tokenViews(leakyList));
  assert.ok(strings.length > 0);
  for (const value of strings) {
    assert.ok(!value.includes(FIXTURE_VALUE), `rendered: ${value}`);
  }
  // And it does show the four things the reader needs, including a revoke
  // button that knows which prefix it revokes.
  assert.ok(strings.includes("laptop"));
  assert.ok(strings.includes("lb_a1b2"));
  assert.ok(strings.includes("Full access"));
  assert.ok(strings.includes("2026-03-04"));
  assert.ok(strings.includes("Revoke the token laptop"));
});

test("only the create response exposes the value, and only to the reveal", () => {
  const created = { token: FIXTURE_VALUE, prefix: "lb_a1b2", name: "laptop" };
  // The same response read as a list row carries nothing…
  assert.equal("token" in tokenViews({ tokens: [created] })[0], false);
  // …but the create view does, and it is written to exactly one element.
  const node = { textContent: "" };
  assert.equal(revealToken(node, createdTokenView(created)).textContent, FIXTURE_VALUE);
  // Dismissing it — or leaving the page — takes the value back out.
  assert.equal(clearToken(node).textContent, "");
});

test("nothing in the token module reaches for storage", () => {
  // The value lives as long as the reader is looking at it, and no longer. The
  // module says so in prose, so the check is on the code: comments are stripped
  // first, or the sentence explaining the rule would fail it.
  const code = readFileSync(new URL("../assets/tokens.js", import.meta.url), "utf8")
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .split("\n")
    .filter((line) => !line.trimStart().startsWith("//"))
    .join("\n");
  for (const forbidden of ["localStorage", "sessionStorage", "indexedDB", "document.cookie"]) {
    assert.ok(!code.includes(forbidden), `tokens.js uses ${forbidden}`);
  }
});

test("return_to accepts a same-origin path, including the device-approval page", () => {
  assert.equal(
    safeReturnTo("/v1/device-auth/approve?user_code=ABCD-EFGH"),
    "/v1/device-auth/approve?user_code=ABCD-EFGH",
  );
  assert.equal(safeReturnTo("/index.html"), "/index.html");
  assert.equal(safeReturnTo("/settings.html#tokens"), "/settings.html#tokens");
});

test("return_to refuses anything that could leave the origin or break out", () => {
  const refused = [
    "//evil.com", // protocol-relative: another origin
    "/\\evil.com", // parsed as `//` by the URL parser
    "https://evil.com", // absolute URL
    "javascript:alert(1)", // a scheme, not a path
    "", // nothing to go to
    "v1/device-auth/approve", // relative without the leading slash
    "/next\n//evil.com", // a control character
    "/next\t/evil.com", // so is a tab
    null,
    undefined,
  ];
  for (const value of refused) {
    assert.equal(safeReturnTo(value), null, JSON.stringify(value));
  }
});

test("return_to is judged after the query decodes it, as the page receives it", () => {
  // The hook percent-encodes the value (`…return_to=%2Fv1%2F…`), and
  // `URLSearchParams` has decoded it by the time `safeReturnTo` runs — so an
  // encoded `//` is refused exactly like a literal one, and the encoded
  // approval page survives the round trip intact.
  const encoded = new URLSearchParams("?return_to=%2F%2Fevil.com");
  assert.equal(safeReturnTo(encoded.get("return_to")), null);
  const approval = new URLSearchParams(
    "?return_to=%2Fv1%2Fdevice-auth%2Fapprove%3Fuser_code%3DABCD-EFGH",
  );
  assert.equal(
    safeReturnTo(approval.get("return_to")),
    "/v1/device-auth/approve?user_code=ABCD-EFGH",
  );
});