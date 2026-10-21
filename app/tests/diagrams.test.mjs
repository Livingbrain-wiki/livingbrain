// The diagram generator (app/assets/diagrams.js) and the Mermaid fence in the
// Markdown renderer. Pure: no DOM, no Mermaid. Run with `node --test app/tests/`.

import test from "node:test";
import assert from "node:assert/strict";

import {
  MAX_EVENTS,
  MAX_NEIGHBOURS,
  diagramFilename,
  diagramsForPage,
  fontFaceRule,
  flattenFacts,
  flowchartFromSteps,
  mermaidConfig,
  oklchToHex,
  palette,
  relationshipMap,
  sanitizeLabel,
  serializeSvg,
  shortDate,
  slugify,
  stepsFromMarkdown,
  timelineFromFacts,
  timelineSvg,
  wrapText,
} from "../assets/diagrams.js";
import { renderMarkdown } from "../assets/markdown.js";

/** A small graph: a focus page, `count` neighbours, edges from the focus. */
function fixtureGraph({ count = 4, focusLabel = "Ada Lovelace" } = {}) {
  const nodes = [{ id: "page:shared:ada", kind: "person", label: focusLabel, slug: "ada", degree: count }];
  const edges = [];
  for (let i = 0; i < count; i += 1) {
    const kind = i % 3 === 0 ? "person" : i % 3 === 1 ? "project" : "decision";
    const id = `page:shared:n${i}`;
    nodes.push({ id, kind, label: `Node ${i}`, slug: `node-${i}`, degree: i });
    edges.push({ source: "page:shared:ada", target: id, kind: "link" });
  }
  return { nodes, edges };
}

/* ------------------------------------------------------------ colour */

test("oklch tokens convert to hex, and anything else is refused", () => {
  assert.equal(oklchToHex("oklch(1 0 0)"), "#ffffff");
  assert.equal(oklchToHex("oklch(0 0 0)"), "#000000");
  assert.match(oklchToHex("oklch(0.86 0.16 162)"), /^#[0-9a-f]{6}$/);
  assert.equal(oklchToHex("oklch(0.5 0.1 200 / 0.5)") !== null, true);
  assert.equal(oklchToHex("#ABCDEF"), "#abcdef");
  assert.equal(oklchToHex("red"), null);
  assert.equal(oklchToHex(""), null);
});

test("the palette reads the tokens and falls back for a missing one", () => {
  const colours = palette({ "--bg": "oklch(1 0 0)" });
  assert.equal(colours.background, "#ffffff");
  assert.match(colours.accent, /^#[0-9a-f]{6}$/);
});

test("the Mermaid config is strict and has no HTML labels", () => {
  const config = mermaidConfig(palette({}));
  assert.equal(config.securityLevel, "strict");
  assert.equal(config.htmlLabels, false);
  assert.equal(config.flowchart.htmlLabels, false);
  assert.equal(config.flowchart.useMaxWidth, false, "phones scroll the stage, not shrink it");
});

/* ------------------------------------------------------------ labels */

test("a label is text: markup and syntax characters never reach Mermaid", () => {
  const hostile = `<img src=x onerror="alert(1)"> "quoted" [a](b) {c} | #d; e: f % g`;
  const label = sanitizeLabel(hostile);
  for (const bad of ["<", ">", '"', "[", "]", "(", ")", "{", "}", "|", "#", ";", ":", "%"]) {
    assert.equal(label.includes(bad), false, `label kept ${bad}: ${label}`);
  }
  assert.equal(label.includes("onerror"), true, "the words stay, only the syntax goes");
});

test("control characters, blanks and long labels are handled", () => {
  assert.equal(sanitizeLabel("a\u0000b\u0007c\n\td"), "a b c d");
  assert.equal(sanitizeLabel("   "), "untitled");
  assert.equal(sanitizeLabel(null), "untitled");
  const long = sanitizeLabel("x".repeat(200), 20);
  assert.equal(long.length, 20);
  assert.equal(long.endsWith("…"), true);
});

test("slugs and file names are lowercase ASCII and never empty", () => {
  assert.equal(slugify("EarthOS.world — Ñandú"), "earthos-world-nandu");
  assert.equal(slugify("   "), "diagram");
  assert.equal(slugify("x".repeat(200)).length, 80);
  assert.equal(
    diagramFilename({ title: "Ada Lovelace", type: "Relationship map", ext: "svg" }),
    "ada-lovelace-relationship-map.svg",
  );
  assert.equal(diagramFilename({ title: "", type: "Timeline", ext: "png" }), "brain-timeline.png");
  assert.equal(diagramFilename({ title: "../../etc/passwd", type: "Flow", ext: "svg" }), "etc-passwd-flow.svg");
});

/* ------------------------------------------------------------ relationship map */

test("a relationship map puts the entity in the centre and groups its neighbours by kind", () => {
  const map = relationshipMap({ focusId: "page:shared:ada", ...fixtureGraph({ count: 4 }) });
  assert.equal(map.type, "relationship");
  assert.match(map.source, /^flowchart LR\n {2}n0\(\["Ada Lovelace"\]\)/);
  assert.equal(map.count, 4);
  assert.ok(map.source.includes('subgraph g0["People"]'));
  assert.ok(map.source.includes('subgraph g1["Projects"]'));
  assert.ok(map.source.indexOf('"People"') < map.source.indexOf('"Projects"'), "groups in a fixed order");
  assert.ok(map.source.includes("n0 --> n1"), "outgoing edges point away from the focus");
});

test("the map caps neighbours and lists the linked ones", () => {
  const map = relationshipMap({ focusId: "page:shared:ada", ...fixtureGraph({ count: 30 }) });
  assert.equal(map.count, MAX_NEIGHBOURS);
  assert.equal(Object.keys(map.links).length, MAX_NEIGHBOURS);
  assert.equal(map.items.length, MAX_NEIGHBOURS);
  assert.deepEqual(new Set(Object.values(map.links)), new Set(map.items.map((item) => item.slug)));
});

test("a map with too few neighbours, or no focus, is skipped", () => {
  assert.equal(relationshipMap({ focusId: "page:shared:ada", ...fixtureGraph({ count: 1 }) }), null);
  assert.equal(relationshipMap({ focusId: "page:shared:missing", ...fixtureGraph({ count: 5 }) }), null);
  assert.equal(relationshipMap({ focusId: "page:shared:ada", nodes: [], edges: [] }), null);
});

test("the same graph gives the same source, whatever order it arrives in", () => {
  const graph = fixtureGraph({ count: 9 });
  const shuffled = { nodes: [...graph.nodes].reverse(), edges: [...graph.edges].reverse() };
  const a = relationshipMap({ focusId: "page:shared:ada", ...graph });
  const b = relationshipMap({ focusId: "page:shared:ada", ...shuffled });
  assert.equal(a.source, b.source);
  assert.deepEqual(a.links, b.links);
});

test("duplicate edges count once and self-links are ignored", () => {
  const graph = fixtureGraph({ count: 3 });
  graph.edges.push(...graph.edges, { source: "page:shared:ada", target: "page:shared:ada", kind: "link" });
  const map = relationshipMap({ focusId: "page:shared:ada", ...graph });
  assert.equal(map.count, 3);
});

test("hostile node labels are escaped before they reach the source", () => {
  const graph = fixtureGraph({ count: 3, focusLabel: '<script>alert(1)</script>"x"' });
  graph.nodes[1].label = '"];click n1 call alert()';
  const map = relationshipMap({ focusId: "page:shared:ada", ...graph });
  assert.equal(map.source.includes("<"), false);
  assert.equal(map.source.includes("click n1"), true, "text survives");
  const labelLines = map.source.split("\n").filter((line) => line.includes('["'));
  for (const line of labelLines) {
    const inner = line.slice(line.indexOf('["') + 2, line.lastIndexOf('"]'));
    assert.equal(inner.includes('"'), false, line);
  }
});

/* ------------------------------------------------------------ steps / flow */

test("ordered steps become a flowchart, skipping code and link markup", () => {
  const markdown = [
    "# Deploy",
    "",
    "1. Build the [image](https://example.com/x)",
    "2. **Push** the tag",
    "3. Roll out `prod`",
    "",
    "```",
    "1. not a step",
    "2. nor this",
    "3. nor this",
    "```",
  ].join("\n");
  const steps = stepsFromMarkdown(markdown);
  assert.deepEqual(steps, ["Build the image", "Push the tag", "Roll out prod"]);
  const flow = flowchartFromSteps(steps);
  assert.equal(flow.type, "flow");
  assert.ok(flow.source.startsWith("flowchart TD"));
  assert.ok(flow.source.includes("s0 --> s1"));
  assert.ok(flow.source.includes("s1 --> s2"));
});

test("two steps, or a list that is not a run of steps, is not a flow", () => {
  assert.equal(flowchartFromSteps(["one", "two"]), null);
  assert.deepEqual(stepsFromMarkdown("1. a\n2. b\n\nprose\n\n1. c\n2. d\n"), []);
});

/* ------------------------------------------------------------ timeline */

function fact(attribute, valid_from, value = "x") {
  return { attribute, value, valid_from };
}

test("three dated facts make a timeline, in date order with year sections", () => {
  const timeline = timelineFromFacts([
    fact("launched", "2026-03-01", "beta"),
    fact("pricing", "2025-11", "free"),
    fact("owner", "2025-06-15", "Ada"),
    fact("undated", null, "skipped"),
  ]);
  assert.equal(timeline.type, "timeline");
  const lines = timeline.source.split("\n");
  assert.equal(lines[0], "timeline");
  assert.ok(lines.indexOf("  section 2025") < lines.indexOf("  section 2026"));
  assert.ok(lines.indexOf("    2025-06-15 : owner Ada") < lines.indexOf("    2025-11 : pricing free"));
  assert.equal(timeline.source.includes("undated"), false);
});

test("fewer than three dated events is no timeline", () => {
  assert.equal(timelineFromFacts([fact("a", "2025-01"), fact("b", "2025-02")]), null);
  assert.equal(timelineFromFacts([fact("a", "2025-01"), fact("a", "2025-01")]), null, "duplicates count once");
  assert.equal(timelineFromFacts([]), null);
});

test("a timeline keeps the newest events and drops a bad date", () => {
  const facts = Array.from({ length: 30 }, (_, i) => fact(`event ${i}`, `2025-${String((i % 12) + 1).padStart(2, "0")}-01`));
  facts.push(fact("bad", "not-a-date"));
  const timeline = timelineFromFacts(facts);
  assert.equal(timeline.count, MAX_EVENTS);
  assert.equal(timeline.source.includes("bad"), false);
});

test("timeline text is escaped", () => {
  const timeline = timelineFromFacts([
    fact("<b>x</b>", "2025-01", 'a: "b" #c'),
    fact("two", "2025-02"),
    fact("three", "2025-03"),
  ]);
  assert.equal(timeline.source.includes("<"), false);
  assert.equal(timeline.source.includes('"'), false);
  assert.equal(timeline.source.includes("#"), false);
});

test("flattenFacts reads current and history from the page's keys", () => {
  const flat = flattenFacts([
    { attribute: "owner", current: [{ value: "Ada", valid_from: "2025-01" }], history: [{ value: "Grace" }] },
    null,
  ]);
  assert.equal(flat.length, 2);
  assert.equal(flat[1].attribute, "owner");
});

/* ------------------------------------------------------------ per page */

test("a page gets at most two diagrams, and a thin page gets none", () => {
  const graph = fixtureGraph({ count: 5 });
  const facts = [fact("a", "2025-01"), fact("b", "2025-02"), fact("c", "2025-03")];
  const steps = "1. one\n2. two\n3. three\n";
  const out = diagramsForPage({
    entityType: "system",
    title: "Deploy pipeline",
    markdown: steps,
    graph,
    focusId: "page:shared:ada",
    facts,
  });
  assert.equal(out.length, 2);
  assert.deepEqual(out.map((d) => d.label), ["Relationship map", "Flow"]);

  assert.deepEqual(
    diagramsForPage({ entityType: "session", title: "x", markdown: "", graph: null, focusId: null, facts: [] }),
    [],
  );
});

test("a page with one neighbour and two facts gets nothing", () => {
  const out = diagramsForPage({
    entityType: "person",
    title: "Ada",
    markdown: "",
    graph: fixtureGraph({ count: 1 }),
    focusId: "page:shared:ada",
    facts: [fact("a", "2025-01"), fact("b", "2025-02")],
  });
  assert.deepEqual(out, []);
});

test("the same inputs give byte-identical diagram sources", () => {
  const args = {
    entityType: "project",
    title: "Ada",
    markdown: "",
    graph: fixtureGraph({ count: 6 }),
    focusId: "page:shared:ada",
    facts: [fact("a", "2025-01"), fact("b", "2025-02"), fact("c", "2025-03")],
  };
  assert.deepEqual(
    diagramsForPage(args).map((d) => d.source),
    diagramsForPage(args).map((d) => d.source),
  );
});

/* ------------------------------------------------------------ export */

const SVG = `<svg id="my-svg" width="100%" xmlns="http://www.w3.org/2000/svg" style="max-width: 100px;" viewBox="0 0 100 50" role="graphics-document"><style>.node{fill:red}</style><g class="node"/></svg>`;

test("an export SVG is padded, filled, sized and carries its font", () => {
  const out = serializeSvg(SVG, { pad: 10, background: "#101010", fontCss: "@font-face{}" });
  assert.match(out, /^<\?xml version="1.0" encoding="UTF-8"\?>\n<svg /);
  assert.ok(out.includes('viewBox="-10 -10 120 70"'));
  assert.ok(out.includes('width="120"') && out.includes('height="70"'));
  assert.equal(out.includes('width="100%"'), false, "the screen width does not survive");
  assert.equal(out.includes("max-width"), false);
  assert.ok(out.includes('<rect x="-10" y="-10" width="120" height="70" fill="#101010"/>'));
  assert.ok(out.includes("<style>@font-face{}</style>"));
  assert.ok(out.includes('id="my-svg"'), "other attributes are kept");
  assert.ok(out.includes('<g class="node"/>'), "the diagram body is kept");
  assert.ok(out.trimEnd().endsWith("</svg>"));
});

test("an export without a usable viewBox or svg is refused", () => {
  assert.equal(serializeSvg("<div>no</div>"), null);
  assert.equal(serializeSvg('<svg width="10"></svg>'), null);
  assert.equal(serializeSvg('<svg viewBox="0 0 0 10"></svg>'), null);
  assert.equal(serializeSvg(""), null);
});

test("an export without fonts still has a background and no style element", () => {
  const out = serializeSvg(SVG, { background: "#fff" });
  assert.equal(out.includes("<style>@font"), false);
  assert.ok(out.includes('fill="#fff"'));
});

test("the inlined font is a data URI rule", () => {
  const rule = fontFaceRule("Hanken Grotesk", "AAAA");
  assert.equal(
    rule,
    '@font-face{font-family:"Hanken Grotesk";font-style:normal;font-weight:400 700;src:url(data:font/woff2;base64,AAAA) format("woff2");}',
  );
});

/* ------------------------------------------------------------ markdown fence */

test("a mermaid fence becomes a figure whose code block is the fallback", () => {
  const [block] = renderMarkdown("```mermaid\nflowchart LR\n  a --> b\n```\n");
  assert.equal(block.tag, "figure");
  assert.equal(block.attrs.class, "mermaid-block");
  assert.equal(block.attrs["data-lang"], "mermaid");
  assert.equal(block.children[0].tag, "pre");
  assert.equal(block.children[0].children[0].children[0].value, "flowchart LR\n  a --> b");
});

test("other fences are unchanged, and a mermaid body stays text", () => {
  const [pre] = renderMarkdown("```js\nlet x = 1;\n```\n");
  assert.equal(pre.tag, "pre");
  const [block] = renderMarkdown("```mermaid\n<script>alert(1)</script>\n```\n");
  const leaf = block.children[0].children[0].children[0];
  assert.equal(leaf.type, "text", "the body is one text node, never markup");
  assert.equal(leaf.value, "<script>alert(1)</script>");
});

/* ------------------------------------------------------------ timeline drawing */

test("dates are short: year, month and year, or day month year", () => {
  assert.equal(shortDate("1842"), "1842");
  assert.equal(shortDate("1842-07"), "Jul 1842");
  assert.equal(shortDate("2024-03-15"), "15 Mar 2024");
});

test("text wraps to a width and ends with an ellipsis past the last line", () => {
  assert.deepEqual(wrapText("joined project Notes", 12, 3), ["joined", "project", "Notes"]);
  const lines = wrapText("one two three four five six seven eight nine ten", 10, 2);
  assert.equal(lines.length, 2);
  assert.ok(lines[1].endsWith("…"));
  assert.ok(lines.every((line) => line.length <= 10));
  assert.deepEqual(wrapText("short", 17, 3), ["short"]);
});

test("the timeline SVG has one dot and one date per event, no style attributes, and an exact viewBox", () => {
  const events = [
    { short: "1 Jan 1840", text: "role Student" },
    { short: "Jul 1842", text: "role <b>Analyst</b> & more" },
    { short: "1843", text: "first algorithm" },
  ];
  const svg = timelineSvg(events, { line: "#111111", accent: "#22cc88", background: "#000000", ink: "#ffffff", ink2: "#cccccc" });
  assert.equal(svg.match(/<circle /g).length, 3, "one dot per event");
  assert.ok(svg.includes('viewBox="0 0 390 158"'), "three columns of 130 and the fixed height");
  assert.equal(svg.includes("style="), false, "presentation attributes only, so the page policy allows it");
  assert.equal(svg.includes("<b>"), false, "event text is escaped");
  assert.ok(svg.includes("&lt;b&gt;"));
  assert.equal(svg.includes("<title"), false);
  assert.equal(timelineSvg([], { line: "#000" }), null);
});

test("the timeline from facts carries events, and its diagram is drawn SVG", () => {
  const out = diagramsForPage({
    entityType: "topic",
    title: "History",
    markdown: "",
    graph: null,
    focusId: null,
    facts: [fact("a", "2025-01"), fact("b", "2025-02-03"), fact("c", "2025")],
    colours: palette({}),
  });
  assert.equal(out.length, 1);
  assert.equal(out[0].label, "Timeline");
  assert.equal(out[0].events.length, 3);
  assert.deepEqual(out[0].events.map((e) => e.short), ["2025", "Jan 2025", "3 Feb 2025"]);
  assert.ok(out[0].svg.startsWith("<svg"));
  assert.equal(out[0].items[0].group, "");
});

test("every relationship arrow leaves the focus, so the map is a fan with no crossings", () => {
  const map = relationshipMap({ focusId: "page:shared:ada", ...fixtureGraph({ count: 5 }), colours: palette({}) });
  const edges = map.source.split("\n").filter((line) => line.includes("-->"));
  assert.equal(edges.length, 5);
  for (const line of edges) assert.match(line, /^ {2}n0 --> n\d+$/);
  assert.equal(map.highlight.id, "n0");
});
