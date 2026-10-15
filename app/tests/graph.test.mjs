// The brain map's data shaping (`assets/graph.js`): the API answer becomes
// the renderer's model, the filters and the time slider pick what shows,
// search finds nodes, and the layout keeps every node inside the brain —
// knowledge in one hemisphere, sources in the other. Plus the page's static
// contract: brain.html is a shell page, and nothing it loads leaves 'self'.

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync, existsSync } from "node:fs";

import {
  KINDS,
  graphPath,
  normalizeGraph,
  visibleMask,
  searchNodes,
  nodeHref,
  tooltipLines,
  prettyLabel,
  nodeWeight,
  createLayout,
  constrain,
  envelopeRadius,
  hemisphere,
  BRAIN,
  rng,
} from "../assets/graph.js";
import { panelMarkdown } from "../assets/brain.js";

const ANSWER = {
  nodes: [
    { id: "page:s:kestrel-routes", kind: "system", label: "kestrel-routes", slug: "kestrel-routes", created_at: "2026-01-10T00:00:00Z", updated_at: "2026-03-01T00:00:00Z", degree: 3, backlinks: 2, facts: 1 },
    { id: "page:s:ada", kind: "person", label: "ada", slug: "ada", created_at: "2026-02-01T00:00:00Z", updated_at: "2026-02-01T00:00:00Z", degree: 1 },
    { id: "page:s:coding-agents--claude--demo--decisions", kind: "coding-agents", label: "coding-agents--claude--demo--decisions", slug: "coding-agents--claude--demo--decisions", project: "demo", agent: "claude", created_at: "2026-04-01T00:00:00Z", degree: 2 },
    { id: "project:demo", kind: "project", label: "Demo", project: "demo", created_at: "2026-03-15T00:00:00Z", degree: 2 },
    { id: "source:01S", kind: "session", label: "claude_code session abc", project: "demo", agent: "claude_code", created_at: "2026-04-01T00:00:00Z", url: "wiki.html?view=sessions&source=01S", degree: 2 },
    { id: "fact:system/ghost", kind: "fact", label: "system/ghost", facts: 2 },
    { id: "weird", kind: "planet", label: "odd" },
  ],
  edges: [
    { source: "page:s:kestrel-routes", target: "page:s:ada", kind: "link" },
    { source: "page:s:ada", target: "page:s:kestrel-routes", kind: "link" },
    { source: "project:demo", target: "source:01S", kind: "member" },
    { source: "project:demo", target: "page:s:coding-agents--claude--demo--decisions", kind: "member" },
    { source: "page:s:coding-agents--claude--demo--decisions", target: "source:01S", kind: "cites" },
    { source: "page:s:kestrel-routes", target: "page:gone", kind: "link" },
  ],
  projects: [{ id: "demo", name: "Demo" }],
  counts: { nodes_total: 9 },
  truncated: true,
};

test("graphPath asks for the filter, and the limit, and nothing else", () => {
  assert.equal(graphPath(), "/v1/graph?limit=2000");
  assert.equal(graphPath({ project: "demo", kind: "session,person", limit: 50 }), "/v1/graph?project=demo&kind=session%2Cperson&limit=50");
});

test("normalizeGraph: indexed nodes, deduplicated edges, facets and the time range", () => {
  const graph = normalizeGraph(ANSWER);
  assert.equal(graph.nodes.length, 7);
  assert.equal(graph.nodes[6].kind, "topic", "an unknown kind is drawn as a topic");
  // Two links between the same pair are one edge; an edge to a node the
  // answer did not carry is dropped.
  assert.equal(graph.edges.length, 4);
  for (const edge of graph.edges) {
    assert.ok(graph.nodes[edge.a] && graph.nodes[edge.b]);
  }
  assert.deepEqual(graph.projects, [{ id: "demo", name: "Demo" }]);
  assert.deepEqual(graph.agents, ["claude", "claude_code"]);
  assert.deepEqual(graph.kinds, ["project", "person", "system", "topic", "coding-agents", "session", "fact"]);
  assert.equal(graph.start, Date.parse("2026-01-10T00:00:00Z"));
  assert.equal(graph.end, Date.parse("2026-04-01T00:00:00Z"));
  assert.equal(graph.truncated, true);
  // Every kind has a colour token.
  const css = readFileSync(new URL("../assets/app.css", import.meta.url), "utf8");
  for (const entry of KINDS) assert.match(css, new RegExp(`${entry.token}:`), entry.token);
});

test("labels read as words; weights grow with links and stay bounded", () => {
  assert.equal(prettyLabel({ kind: "system", label: "kestrel-routes" }), "Kestrel routes");
  assert.equal(prettyLabel({ kind: "coding-agents", label: "coding-agents--claude--demo--open-questions" }), "Open questions");
  assert.equal(prettyLabel({ kind: "session", label: "claude_code session abc" }), "claude_code · abc");
  assert.equal(prettyLabel({ kind: "fact", label: "system/ghost" }), "system/ghost");
  const small = nodeWeight({ kind: "person", degree: 0 });
  const hub = nodeWeight({ kind: "person", degree: 40, backlinks: 30 });
  assert.ok(hub > small);
  assert.ok(nodeWeight({ kind: "person", degree: 1e6, backlinks: 1e6 }) <= 3.2);
});

test("visibleMask: kinds, project, agent and the time slider combine", () => {
  const graph = normalizeGraph(ANSWER);
  const shown = (filters) =>
    [...visibleMask(graph, filters)].map((on, at) => (on ? graph.nodes[at].id : null)).filter(Boolean);
  assert.equal(shown({}).length, 7);
  assert.deepEqual(shown({ kinds: new Set(["person"]) }), ["page:s:ada"]);
  assert.deepEqual(shown({ project: "demo" }), [
    "page:s:coding-agents--claude--demo--decisions",
    "project:demo",
    "source:01S",
  ]);
  assert.deepEqual(shown({ agent: "claude_code" }), ["source:01S"]);
  // As of February: what was created by then, and anything undated.
  const until = Date.parse("2026-02-15T00:00:00Z");
  assert.deepEqual(shown({ until }), ["page:s:kestrel-routes", "page:s:ada", "fact:system/ghost", "weird"]);
});

test("searchNodes: label prefix first, then substring, then slug or project", () => {
  const graph = normalizeGraph(ANSWER);
  assert.deepEqual(searchNodes(graph, "kes").map((n) => n.id), ["page:s:kestrel-routes"]);
  assert.deepEqual(searchNodes(graph, "demo").map((n) => n.id).slice(0, 1), ["project:demo"]);
  assert.deepEqual(searchNodes(graph, "  "), []);
});

test("nodeHref and the tooltip say where a node opens and what it is", () => {
  const graph = normalizeGraph(ANSWER);
  const byId = (id) => graph.nodes[graph.index.get(id)];
  assert.equal(nodeHref(byId("page:s:kestrel-routes")), "wiki.html?slug=kestrel-routes");
  assert.equal(nodeHref(byId("source:01S")), "wiki.html?view=sessions&source=01S");
  assert.equal(nodeHref(byId("project:demo")), "wiki.html?view=sessions&project=demo");
  assert.equal(nodeHref(byId("fact:system/ghost")), "wiki.html?view=facts");
  assert.deepEqual(tooltipLines(byId("page:s:kestrel-routes")), [
    "Kestrel routes",
    "System",
    "2026-01-10 → 2026-03-01",
    "2 backlinks · 1 fact",
  ]);
  assert.deepEqual(tooltipLines(byId("source:01S")).slice(0, 2), ["claude_code · abc", "Agent session · demo"]);
});

test("the layout keeps every node inside the brain, sources in the right hemisphere", () => {
  const nodes = [];
  const edges = [];
  const r = rng(5);
  for (let i = 0; i < 400; i += 1) {
    nodes.push({ id: `n${i}`, kind: i % 4 === 0 ? "session" : "topic" });
    if (i > 0) edges.push({ source: `n${i}`, target: `n${Math.floor(r() * i)}` });
  }
  const graph = normalizeGraph({ nodes, edges });
  const layout = createLayout(graph, { seed: 3 });
  let going = true;
  let guard = 0;
  while (going && guard < 1000) {
    going = layout.run(20);
    guard += 20;
  }
  assert.equal(going, false, "the layout settles");
  const pos = layout.positions;
  graph.nodes.forEach((node, i) => {
    const p = { x: pos[i * 3], y: pos[i * 3 + 1], z: pos[i * 3 + 2] };
    const side = hemisphere(node);
    assert.ok(envelopeRadius(p, side) <= 1.0001, `${node.id} is inside the envelope`);
    assert.ok(side * p.x >= 0.05 - 1e-6, `${node.id} keeps to its hemisphere`);
    assert.ok(Number.isFinite(p.x + p.y + p.z));
  });
  // Repeatable: the same graph lays out the same way.
  const again = createLayout(graph, { seed: 3 });
  again.run(5);
  const first = createLayout(graph, { seed: 3 });
  first.run(5);
  assert.deepEqual([...again.positions.slice(0, 9)], [...first.positions.slice(0, 9)]);
});

test("constrain pulls a stray point back onto the envelope", () => {
  const p = constrain({ x: 5, y: 5, z: 5 }, -1);
  assert.ok(envelopeRadius(p, -1) <= 1.0001);
  const q = constrain({ x: -0.01, y: 0, z: 0 }, 1);
  assert.equal(q.x, 0.05, "the fissure stays open");
  assert.ok(BRAIN.rz > BRAIN.ry && BRAIN.ry > BRAIN.rx, "longer front to back than tall, taller than wide");
});

test("the side panel drops the fence and the repeated title", () => {
  assert.equal(panelMarkdown("---\ntitle: K\n---\n\n# Kestrel\n\nBody."), "Body.");
  assert.equal(panelMarkdown("Body only."), "Body only.");
  assert.equal(panelMarkdown(null), "");
});

test("brain.html is a shell page whose scripts and styles are all its own", () => {
  const html = readFileSync(new URL("../brain.html", import.meta.url), "utf8");
  assert.match(html, /<body data-page="brain">/);
  assert.match(html, /<nav id="app-nav" class="sidebar__nav" aria-label="Main">/);
  for (const [, ref] of html.matchAll(/(?:src|href)="(assets\/[^"]+)"/g)) {
    assert.ok(existsSync(new URL(`../${ref}`, import.meta.url)), ref);
  }
  assert.doesNotMatch(html, /src="https?:/, "no third-party script");
  for (const id of ["map-canvas", "map-panel", "map-time", "map-list", "map-search", "map-legend"]) {
    assert.match(html, new RegExp(`id="${id}"`), id);
  }
  const headers = readFileSync(new URL("../_headers", import.meta.url), "utf8");
  assert.match(headers, /\/brain\.html\n  Cache-Control: private, no-cache/);
  const scriptSrc = /script-src ([^;]*);/.exec(headers)[1];
  assert.doesNotMatch(scriptSrc, /unsafe|https?:|\*/, "scripts stay 'self' (and the one hash)");
  const vendor = readFileSync(new URL("../assets/vendor/VENDOR.md", import.meta.url), "utf8");
  assert.match(vendor, /7a509b321ef39c65f9a2951e54f4347c8ac195d1a64ac47adf00b188d39dd708/);
});
