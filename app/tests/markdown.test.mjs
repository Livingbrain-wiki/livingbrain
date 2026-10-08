// The Markdown pipeline, the citation renderer and the backlinks renderer.
//
// These import the same module the page imports (`../assets/markdown.js`),
// so a change that breaks the renderer breaks this file too. Run with
// `node --test app/tests/`.

import test from "node:test";
import assert from "node:assert/strict";

import {
  renderMarkdown,
  renderCitation,
  renderCitations,
  renderBacklinks,
  isSafeHref,
  parseInline,
} from "../assets/markdown.js";

/** Every node in a tree, depth first. */
function walk(node, out = []) {
  out.push(node);
  for (const child of node.children || []) walk(child, out);
  return out;
}

/** A compact string form, used to assert on the shape without noise. */
function shape(nodes) {
  return nodes
    .map((n) =>
      n.type === "text"
        ? `text(${JSON.stringify(n.value)})`
        : `${n.tag}(${shape(n.children || [])})${
            n.attrs ? JSON.stringify(n.attrs) : ""
          }`,
    )
    .join(" ");
}

/** The concatenated text of a tree — what a person actually reads. */
function plain(node) {
  if (node.type === "text") return node.value;
  return (node.children || []).map(plain).join("");
}

test("headings, paragraphs and lists become elements", () => {
  const blocks = renderMarkdown("# Title\n\nA line.\n\n- one\n- two\n");
  assert.equal(blocks.length, 3);
  assert.equal(blocks[0].tag, "h1");
  assert.equal(plain(blocks[0]), "Title");
  assert.equal(blocks[1].tag, "p");
  assert.equal(blocks[2].tag, "ul");
  assert.equal(blocks[2].children.length, 2);
  assert.equal(plain(blocks[2].children[1]), "two");
});

test("ordered lists, quotes, rules and fences", () => {
  const blocks = renderMarkdown(
    "1. first\n2. second\n\n> quoted\n\n---\n\n```rust\nlet x = 1;\n```\n",
  );
  const tags = blocks.map((b) => b.tag);
  assert.deepEqual(tags, ["ol", "blockquote", "hr", "pre"]);
  assert.equal(blocks[0].children.length, 2);
  assert.equal(plain(blocks[3].children[0]), "let x = 1;");
  assert.equal(blocks[3].attrs["data-lang"], "rust");
});

test("inline code, strong, emphasis and links", () => {
  const runs = parseInline("a `code` **bold** *em* [docs](https://x.test/d)");
  const tags = runs.filter((r) => r.type === "element").map((r) => r.tag);
  assert.deepEqual(tags, ["code", "strong", "em", "a"]);
  const link = runs.find((r) => r.tag === "a");
  assert.equal(link.attrs.href, "https://x.test/d");
  assert.equal(link.attrs.rel, "noopener noreferrer");
  assert.equal(plain(link), "docs");
});

test("page content can never emit executable HTML", () => {
  // Every one of these is something a wiki page body might contain.
  const hostile = [
    "<script>alert(1)</script>",
    "<img src=x onerror=alert(1)>",
    "<iframe src='javascript:alert(1)'></iframe>",
    "# <script>alert(1)</script>",
    "- <svg onload=alert(1)>",
    "> <script>alert(1)</script>",
    "[click](javascript:alert(1))",
    "[click](JaVaScRiPt:alert(1))",
    "[click](java\nscript:alert(1))",
    "<a href='javascript:alert(1)'>click</a>",
    "![x](javascript:alert(1))",
  ];
  for (const source of hostile) {
    const blocks = renderMarkdown(source);
    for (const node of blocks.flatMap((b) => walk(b))) {
      if (node.type === "text") continue;
      // No renderer path may produce a script, an event handler, a frame or
      // an image: the only tags it names are these.
      assert.ok(
        [
          "p", "h1", "h2", "h3", "h4", "h5", "h6", "ul", "ol", "li", "pre",
          "code", "strong", "em", "blockquote", "hr", "a", "span",
        ].includes(node.tag),
        `unexpected tag ${node.tag} from ${source}`,
      );
      for (const [name, value] of Object.entries(node.attrs || {})) {
        assert.ok(
          !name.startsWith("on"),
          `event handler attribute ${name} from ${source}`,
        );
        if (name === "href") {
          assert.ok(
            isSafeHref(value),
            `unsafe href survived: ${value} from ${source}`,
          );
        }
      }
      // The markup the person typed survives as visible text.
      if (source.includes("<script>")) {
        assert.match(plain(node), /<script>/);
      }
    }
  }
});

test("an unsafe link keeps its words and loses its href", () => {
  const blocks = renderMarkdown("[click me](javascript:alert(1))");
  const nodes = walk(blocks[0]);
  assert.equal(nodes.filter((n) => n.tag === "a").length, 0);
  assert.match(plain(blocks[0]), /click me/);
});

test("isSafeHref allows http, https, mailto and relative paths", () => {
  for (const good of [
    "https://x.test",
    "http://x.test/a?b=c",
    "mailto:a@b.test",
    "/v1/pages/x",
    "wiki.html?slug=x",
    "#section",
  ]) {
    assert.ok(isSafeHref(good), `${good} should be allowed`);
  }
  for (const bad of [
    "javascript:alert(1)",
    "JAVASCRIPT:alert(1)",
    "data:text/html,<script>alert(1)</script>",
    "vbscript:msgbox(1)",
    "java\nscript:alert(1)",
    "",
    "   ",
  ]) {
    assert.ok(!isSafeHref(bad), `${bad} should be refused`);
  }
});

test("a citation renders title, quote and url", () => {
  const node = renderCitation({
    title: "RFC 9110",
    url: "https://example.test/rfc",
    quote: "the method is safe",
  });
  assert.equal(node.tag, "li");
  const link = walk(node).find((n) => n.tag === "a");
  assert.equal(plain(link), "RFC 9110");
  assert.equal(link.attrs.href, "https://example.test/rfc");
  assert.match(shape([node]), /the method is safe/);
  assert.match(shape([node]), /https:\/\/example.test\/rfc/);
});

test("a citation with no url still renders its title", () => {
  const node = renderCitation({ title: "A meeting" });
  assert.equal(walk(node).filter((n) => n.tag === "a").length, 0);
  assert.match(shape([node]), /A meeting/);
});

test("a citation with an unsafe url loses only the link", () => {
  const node = renderCitation({ title: "Bad", url: "javascript:alert(1)" });
  assert.equal(walk(node).filter((n) => n.tag === "a").length, 0);
  assert.match(shape([node]), /Bad/);
});

test("renderCitations hides the list when there is nothing to show", () => {
  assert.equal(renderCitations([]).attrs.hidden, "");
  assert.equal(renderCitations([]).attrs.class, "citations");
  assert.equal(renderCitations([{ title: "x" }]).attrs.hidden, undefined);
  assert.equal(renderCitations(undefined).children.length, 0);
});

test("backlinks render one link per unique slug", () => {
  const node = renderBacklinks(["decisions/2026-10", "people/ada", "", "decisions/2026-10"]);
  assert.equal(node.tag, "ul");
  assert.equal(node.children.length, 2);
  const links = walk(node).filter((n) => n.tag === "a");
  assert.equal(plain(links[0]), "decisions/2026-10");
  assert.equal(links[0].attrs.href, "wiki.html?slug=decisions%2F2026-10");
  assert.equal(node.attrs.hidden, undefined);
});

test("an empty backlink list is hidden, not an empty box", () => {
  assert.equal(renderBacklinks([]).attrs.hidden, "");
  assert.equal(renderBacklinks(null).children.length, 0);
});

test("a slug is escaped in the link, not interpreted", () => {
  const node = renderBacklinks(['x"><script>alert(1)</script>']);
  const link = walk(node).find((n) => n.tag === "a");
  assert.match(link.attrs.href, /%3Cscript%3E/);
  assert.equal(plain(link), 'x"><script>alert(1)</script>');
  assert.equal(link.children[0].type, "text");
});

test("empty and null input is empty output, not a crash", () => {
  assert.deepEqual(renderMarkdown(""), []);
  assert.deepEqual(renderMarkdown(null), []);
  assert.deepEqual(renderMarkdown("   \n\n  "), []);
});