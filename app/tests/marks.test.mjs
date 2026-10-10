// Which mark each provider wears, ported from Colonizer's providerMark.test.tsx
// (minus the URL matching: a stored connection is read back with its provider id
// here, so there is no base URL to resolve a vendor from).
//
// The set of ids with artwork is asserted exactly, so the next
// `scripts/sync-providers` run cannot silently add a vendor that deserves a
// mark but wears initials — the test fails and someone decides.

import test from "node:test";
import assert from "node:assert/strict";

import { PROVIDERS, POPULAR, CUSTOM } from "../assets/models.js";
import { MARKS, MARK_BY_ID, markOf, initialsOf, providerMark } from "../assets/marks.js";

/** A doc that records calls, in the style of the models.test.mjs fake. */
const node = (tag) => ({
  tag,
  className: "",
  textContent: "",
  attrs: {},
  children: [],
  setAttribute(name, value) {
    this.attrs[name] = value;
  },
  appendChild(child) {
    this.children.push(child);
  },
});
const doc = {
  createElement: (tag) => node(tag),
  createElementNS: (_ns, tag) => node(tag),
};

const IDS = [...PROVIDERS.map((p) => p.id), CUSTOM.id];

/** The tile's artwork, or null when it carries initials instead. */
const artworkOf = (id, name = "Whatever") => {
  const tile = providerMark(id, name, doc);
  const svg = tile.children[0];
  return svg && svg.tag === "svg" ? svg : null;
};

test("every catalogue provider resolves to a mark or a non-empty lettermark", () => {
  for (const id of IDS) {
    const name = id === CUSTOM.id ? CUSTOM.name : PROVIDERS.find((p) => p.id === id).name;
    const tile = providerMark(id, name, doc);
    if (markOf(id)) {
      assert.equal(artworkOf(id, name) !== null, true, `${id} renders its mark`);
    } else {
      assert.match(tile.className, /provider-mark--letters/, `${id} falls back to letters`);
      assert.match(tile.textContent, /^[A-Z0-9?]{1,2}$/, `${id}'s initials are one or two capitals`);
    }
  }
});

test("the providers with artwork are exactly these, so a catalog change is noticed", () => {
  const withArtwork = IDS.filter((id) => markOf(id)).sort();
  assert.deepEqual(withArtwork, [
    "anthropic",
    "byteplus",
    "deepseek",
    "github-copilot",
    "kimi",
    "kimi-for-coding",
    "meta",
    "minimax",
    "minimax-en",
    "modelscope",
    "nvidia",
    "openai",
    "openrouter",
    "qianwen-ai",
    "qianwen-coding-plan",
    "qianwen-token-plan",
    "qwencloud",
    "qwencloud-for-coding",
    "qwencloud-token-plan",
    "xai-grok",
    "xiaomi-mimo",
    "xiaomi-mimo-token-plan-china",
    "zhipu-glm",
    "zhipu-glm-en",
  ]);
});

test("the Alibaba family shares one mark, whatever the endpoint is called", () => {
  const family = IDS.filter((id) => /^(qwencloud|alibaba|qianwen)(-|$)/.test(id));
  assert.ok(family.length >= 6, family.join(" "));
  const cloud = markOf("qwencloud");
  for (const id of family) assert.equal(markOf(id), cloud, id);
  assert.equal(cloud.d, MARKS.alibabacloud.d);
});

test("has artwork, not initials, for Z.AI, Zhipu, Meta and BytePlus", () => {
  for (const id of ["zhipu-glm", "zhipu-glm-en", "meta", "byteplus"]) {
    assert.ok(artworkOf(id), `${id} has artwork`);
    assert.doesNotMatch(providerMark(id, "Zhipu AI", doc).className, /provider-mark--letters/, id);
  }
  assert.notEqual(markOf("zhipu-glm-en").d, markOf("meta").d, "Z.AI and Meta are different marks");
});

test("still falls back to initials for a vendor with no mark", () => {
  assert.equal(initialsOf("OpenAI"), "OA");
  assert.equal(initialsOf("Z.AI"), "ZA");
  const unmarked = providerMark("novita-ai", "Novita AI", doc);
  assert.match(unmarked.className, /provider-mark--letters/);
  assert.equal(unmarked.textContent, "NA");
  const custom = providerMark("custom", CUSTOM.name, doc);
  assert.match(custom.className, /provider-mark--letters/);
  assert.equal(custom.textContent, "CE");
  // An id the catalog has never heard of is a lettermark too, never a crash.
  const unknown = providerMark("gone-provider", "Gone Provider", doc);
  assert.equal(unknown.textContent, "GP");
  assert.equal(providerMark(undefined, "", doc).textContent, "?");
});

test("every popular provider wears a real mark, not initials", () => {
  for (const id of POPULAR) {
    assert.ok(artworkOf(id), `${id} has artwork`);
    assert.doesNotMatch(providerMark(id, "Whatever", doc).className, /provider-mark--letters/, id);
  }
});

test("the built tile is decorative, and its SVG is one currentColor path", () => {
  const tile = providerMark("anthropic", "Anthropic", doc);
  assert.equal(tile.className, "provider-mark");
  assert.equal(tile.attrs["aria-hidden"], "true");
  const svg = tile.children[0];
  assert.equal(svg.tag, "svg");
  assert.equal(svg.attrs.viewBox, "0 0 24 24");
  assert.equal(svg.attrs.fill, "currentColor");
  assert.equal(svg.attrs["aria-hidden"], "true");
  assert.equal(svg.attrs.focusable, "false");
  assert.equal(svg.children.length, 1);
  const path = svg.children[0];
  assert.equal(path.tag, "path");
  assert.ok(path.attrs.d.length > 40, "the path data came across");
  assert.equal(path.attrs.fill, undefined, "the SVG carries the fill, the path inherits it");
  // The two marks upstream needed a fill rule for keep it.
  for (const id of ["zhipu-glm", "meta"]) {
    assert.equal(artworkOf(id).children[0].attrs["fill-rule"], "evenodd", id);
  }
  assert.equal(artworkOf("anthropic").children[0].attrs["fill-rule"], undefined);
});

test("no dead artwork: every mark in the table is reachable from the map", () => {
  const used = new Set([...Object.values(MARK_BY_ID), "alibabacloud"]);
  for (const [key, mark] of Object.entries(MARKS)) {
    assert.ok(used.has(key), `${key} is referenced`);
    assert.match(mark.d, /^[\sA-Za-z0-9.,+-]+$/, `${key}'s path data is path data`);
    if (mark.fillRule) assert.equal(mark.fillRule, "evenodd");
  }
  assert.equal(Object.keys(MARKS).length, 15);
});
