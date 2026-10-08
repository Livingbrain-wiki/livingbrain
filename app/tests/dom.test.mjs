// The DOM half of the wiki renderer.
//
// `renderMarkdown` returns node descriptions; `mountNodes` in `app.js` is the
// only thing that turns them into elements. This test gives it a document that
// records every call, so it can assert *how* content reaches the DOM: through
// `createTextNode` and `setAttribute`, and never through an HTML string.
//
// That is the real safety property. `markdown.test.mjs` proves the renderer
// never names a dangerous tag; this proves there is no other way for content
// to become markup even if it did.

import test from "node:test";
import assert from "node:assert/strict";

import { mountNodes } from "../assets/app.js";
import { renderMarkdown, renderCitations, renderBacklinks } from "../assets/markdown.js";

/**
 * Runs `body` with `globalThis.document` swapped for a recorder, so the code
 * under test is `mountNodes` exactly as the page runs it — the same global
 * lookup, no test-only injection path through the app.
 */
function withDocument(doc, body) {
  const original = globalThis.document;
  globalThis.document = doc;
  try {
    return body();
  } finally {
    globalThis.document = original;
  }
}

/**
 * A document that records how it was used. Any method the app does not define
 * here simply cannot be called: `createElement`, `createTextNode` and
 * `createDocumentFragment` are all it offers, so a switch to `innerHTML` would
 * throw rather than pass quietly.
 */
function recordingDocument() {
  const calls = [];
  const make = (tag) => ({
    tag,
    attrs: {},
    children: [],
    text: "",
    hidden: false,
    setAttribute(name, value) {
      calls.push({ op: "setAttribute", tag, name, value });
      this.attrs[name] = value;
    },
    appendChild(child) {
      calls.push({ op: "appendChild", tag, childTag: child.tag || "#text" });
      this.children.push(child);
      return child;
    },
  });
  return {
    calls,
    createElement: (tag) => {
      calls.push({ op: "createElement", tag });
      return make(tag);
    },
    createTextNode(value) {
      calls.push({ op: "createTextNode", value });
      const node = make("#text");
      node.text = String(value);
      return node;
    },
    createDocumentFragment() {
      calls.push({ op: "createDocumentFragment" });
      return make("#fragment");
    },
  };
}

test("a hostile page body only ever becomes text nodes and attributes", () => {
  const source = [
    "# <script>alert(1)</script>",
    "",
    "<img src=x onerror=alert(1)>",
    "",
    "[click](javascript:alert(1))",
    "",
    "[ok](https://example.test/a)",
  ].join("\n");

  const doc = recordingDocument();
  withDocument(doc, () => mountNodes(renderMarkdown(source)));

  const ops = new Set(doc.calls.map((c) => c.op));
  assert.deepEqual(
    [...ops].sort(),
    [
      "appendChild",
      "createDocumentFragment",
      "createElement",
      "createTextNode",
      "setAttribute",
    ],
    "only these document APIs are used",
  );

  // No event-handler attribute was ever set.
  for (const call of doc.calls) {
    if (call.op === "setAttribute") {
      assert.ok(!call.name.startsWith("on"), `set ${call.name}`);
    }
  }

  // The script text survived as a text node, verbatim.
  const textValues = doc.calls.filter((c) => c.op === "createTextNode").map((c) => c.value);
  assert.ok(textValues.some((v) => v.includes("<script>alert(1)</script>")));

  // The one link that was kept is https.
  const hrefs = doc.calls.filter((c) => c.op === "setAttribute" && c.name === "href");
  assert.equal(hrefs.length, 1);
  assert.equal(hrefs[0].value, "https://example.test/a");
});

test("mountNodes never invents a tag the description did not name", () => {
  const doc = recordingDocument();
  withDocument(doc, () => mountNodes(renderMarkdown("# a\n\n- b\n")));
  const tags = doc.calls.filter((c) => c.op === "createElement").map((c) => c.tag);
  assert.deepEqual(tags, ["h1", "ul", "li"]);
});

test("citations and backlinks mount the same safe way", () => {
  for (const nodes of [
    renderCitations([{ title: "<b>x</b>", url: "javascript:alert(1)", quote: "<i>q</i>" }]),
    renderBacklinks(['<script>alert(1)</script>']),
  ]) {
    const doc = recordingDocument();
    withDocument(doc, () => mountNodes(nodes));
    const values = doc.calls.filter((c) => c.op === "createTextNode").map((c) => c.value);
    assert.ok(values.some((v) => v.includes("<")), "markup stays visible text");
    const hrefs = doc.calls.filter((c) => c.op === "setAttribute" && c.name === "href");
    for (const href of hrefs) {
      assert.ok(!/^javascript:/i.test(href.value), href.value);
    }
  }
});

test("the hidden marker is set as a property, not an attribute", () => {
  const doc = recordingDocument();
  const root = withDocument(doc, () => mountNodes(renderBacklinks([])));
  const list = root.children[0];
  assert.equal(list.hidden, true);
  // `hidden` never becomes a string attribute a CSS rule could style around.
  assert.ok(!doc.calls.some((c) => c.op === "setAttribute" && c.name === "hidden"));
});