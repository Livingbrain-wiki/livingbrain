// The brain map's data, shaped (`GET /v1/graph` → what the renderer draws).
//
// Pure: no DOM, no fetch, no canvas — so `tests/graph.test.mjs` pins the
// shaping, the filters, the time slider and the layout's brain envelope
// directly. `brain.js` is the page around it, `brain-render.js` the canvas.

/** The kinds a node can be, in legend order, with the colour token each is
 * drawn in (app.css `--t-*`, the website's palette and its extensions). */
export const KINDS = Object.freeze([
  { kind: "project", label: "Projects", token: "--t-project" },
  { kind: "person", label: "People", token: "--t-person" },
  { kind: "decision", label: "Decisions", token: "--t-decision" },
  { kind: "customer", label: "Customers", token: "--t-customer" },
  { kind: "system", label: "Systems", token: "--t-system" },
  { kind: "topic", label: "Topics", token: "--t-topic" },
  { kind: "glossary", label: "Glossary", token: "--t-topic" },
  { kind: "radar", label: "Radar", token: "--t-topic" },
  { kind: "coding-agents", label: "Coding agents", token: "--t-agents" },
  { kind: "session", label: "Agent sessions", token: "--t-session" },
  { kind: "github", label: "GitHub", token: "--t-github" },
  { kind: "source", label: "Imports & sources", token: "--t-source" },
  { kind: "fact", label: "Fact subjects", token: "--t-fact" },
]);

const KIND_INDEX = new Map(KINDS.map((entry, at) => [entry.kind, at]));

/** The API's map path for a filter. */
export function graphPath({ project = "", kind = "", limit = 2000 } = {}) {
  const params = new URLSearchParams();
  if (project) params.set("project", project);
  if (kind) params.set("kind", kind);
  params.set("limit", String(limit));
  return `/v1/graph?${params.toString()}`;
}

/** "kestrel-routes" → "Kestrel routes"; a compiled page's last segment. */
export function prettyLabel(node) {
  const raw = String((node && node.label) || (node && node.slug) || "");
  if (node && node.kind === "session") return raw.replace(/^([a-z_]+) session /, "$1 · ");
  if (node && (node.kind === "source" || node.kind === "github" || node.kind === "project" || node.kind === "fact")) return raw;
  const last = raw.split("--").pop() || raw;
  const words = last.replace(/-/g, " ").trim();
  return words ? words.charAt(0).toUpperCase() + words.slice(1) : raw;
}

/** A date string to epoch milliseconds, or null. */
export function when(value) {
  if (!value) return null;
  const at = Date.parse(String(value));
  return Number.isFinite(at) ? at : null;
}

/** A node's radius weight: grows with its links, backlinks and facts, on a
 * square-root curve so hubs stand out without swallowing the map. */
export function nodeWeight(node) {
  const degree = Number(node.degree) || 0;
  const backlinks = Number(node.backlinks) || 0;
  const facts = Number(node.facts) || 0;
  const base = node.kind === "project" ? 1.6 : node.kind === "session" || node.kind === "source" ? 0.55 : 0.8;
  return Math.min(3.2, base + Math.sqrt(degree + backlinks * 1.5 + facts * 0.5) * 0.32);
}

/**
 * The API answer as the renderer's model: nodes with an index, their kind
 * index, weight, label and dates; edges as index pairs (edges to a node the
 * answer did not carry are dropped); and the facets — projects, agents,
 * kinds present — and the time range.
 */
export function normalizeGraph(answer) {
  const rawNodes = answer && Array.isArray(answer.nodes) ? answer.nodes : [];
  const nodes = [];
  const index = new Map();
  for (const raw of rawNodes) {
    if (!raw || typeof raw.id !== "string" || index.has(raw.id)) continue;
    const kind = KIND_INDEX.has(raw.kind) ? raw.kind : "topic";
    const node = {
      i: nodes.length,
      id: raw.id,
      kind,
      k: KIND_INDEX.get(kind),
      label: prettyLabel({ ...raw, kind }),
      slug: typeof raw.slug === "string" ? raw.slug : null,
      project: raw.project || null,
      agent: raw.agent || null,
      scope: raw.scope || null,
      created: when(raw.created_at),
      updated: when(raw.updated_at),
      createdAt: raw.created_at || null,
      updatedAt: raw.updated_at || null,
      backlinks: Number(raw.backlinks) || 0,
      facts: Number(raw.facts) || 0,
      degree: Number(raw.degree) || 0,
      url: typeof raw.url === "string" ? raw.url : null,
    };
    node.weight = nodeWeight(node);
    index.set(node.id, node.i);
    nodes.push(node);
  }
  const edges = [];
  const seen = new Set();
  for (const raw of answer && Array.isArray(answer.edges) ? answer.edges : []) {
    const a = index.get(raw && raw.source);
    const b = index.get(raw && raw.target);
    if (a == null || b == null || a === b) continue;
    const key = a < b ? `${a}_${b}` : `${b}_${a}`;
    if (seen.has(key)) continue;
    seen.add(key);
    edges.push({ a, b, kind: String(raw.kind || "link") });
  }
  const times = nodes.map((node) => node.created).filter((t) => t != null);
  const projects = new Map();
  for (const entry of answer && Array.isArray(answer.projects) ? answer.projects : []) {
    if (entry && entry.id) projects.set(entry.id, entry.name || entry.id);
  }
  for (const node of nodes) if (node.project && !projects.has(node.project)) projects.set(node.project, node.project);
  const agents = [...new Set(nodes.map((node) => node.agent).filter(Boolean))].sort();
  const kinds = KINDS.filter((entry) => nodes.some((node) => node.kind === entry.kind)).map((entry) => entry.kind);
  return {
    nodes,
    edges,
    index,
    projects: [...projects].map(([id, name]) => ({ id, name })).sort((a, b) => a.name.localeCompare(b.name)),
    agents,
    kinds,
    start: times.length ? Math.min(...times) : null,
    end: times.length ? Math.max(...times) : null,
    counts: (answer && answer.counts) || null,
    truncated: Boolean(answer && answer.truncated),
  };
}

/**
 * Which nodes a filter shows: `{ kinds: Set|null, project, agent, until }`.
 * `until` (epoch ms) hides what was created later — the time slider. A node
 * with no date is shown at every time. Returns a `Uint8Array` mask.
 */
export function visibleMask(graph, { kinds = null, project = "", agent = "", until = null } = {}) {
  const mask = new Uint8Array(graph.nodes.length);
  graph.nodes.forEach((node, at) => {
    if (kinds && !kinds.has(node.kind)) return;
    if (project && node.project !== project) return;
    if (agent && node.agent !== agent) return;
    if (until != null && node.created != null && node.created > until) return;
    mask[at] = 1;
  });
  return mask;
}

/** The nodes a search matches, best first: label prefix, then substring,
 * then slug/project substring. */
export function searchNodes(graph, query, max = 8) {
  const q = String(query || "").trim().toLowerCase();
  if (!q) return [];
  const scored = [];
  for (const node of graph.nodes) {
    const label = node.label.toLowerCase();
    const extra = `${node.slug || ""} ${node.project || ""} ${node.agent || ""}`.toLowerCase();
    let score = -1;
    if (label.startsWith(q)) score = 0;
    else if (label.includes(q)) score = 1;
    else if (extra.includes(q)) score = 2;
    if (score >= 0) scored.push({ node, score });
  }
  scored.sort((a, b) => a.score - b.score || b.node.weight - a.node.weight);
  return scored.slice(0, max).map((entry) => entry.node);
}

/** Where a node opens: a page in the wiki, a session's transcript. */
export function nodeHref(node) {
  if (!node) return null;
  if (node.slug) return `wiki.html?slug=${encodeURIComponent(node.slug)}`;
  if (node.kind === "session" && node.id.startsWith("source:")) {
    return `wiki.html?view=sessions&source=${encodeURIComponent(node.id.slice(7))}`;
  }
  if (node.kind === "project") return `wiki.html?view=sessions&project=${encodeURIComponent(node.project || "")}`;
  if (node.kind === "fact") return "wiki.html?view=facts";
  return null;
}

/** A node's kind in the singular, for a tooltip or the panel. */
export const KIND_NAMES = Object.freeze({
  project: "Project",
  person: "Person",
  decision: "Decision",
  customer: "Customer",
  system: "System",
  topic: "Topic",
  glossary: "Glossary term",
  radar: "Radar item",
  "coding-agents": "Coding-agent page",
  session: "Agent session",
  github: "GitHub item",
  source: "Import or source",
  fact: "Fact subject",
});

export function kindName(node) {
  return KIND_NAMES[node && node.kind] || String((node && node.kind) || "Node");
}

/** The tooltip's lines: title, kind (· project), dates. */
export function tooltipLines(node) {
  const kind = kindName(node);
  const lines = [node.label, node.project ? `${kind} · ${node.project}` : kind];
  const fmt = (ms) => new Date(ms).toISOString().slice(0, 10);
  if (node.created != null && node.updated != null && node.updated - node.created > 86400000) {
    lines.push(`${fmt(node.created)} → ${fmt(node.updated)}`);
  } else if (node.created != null) {
    lines.push(fmt(node.created));
  }
  const extras = [];
  if (node.backlinks) extras.push(`${node.backlinks} backlink${node.backlinks === 1 ? "" : "s"}`);
  if (node.facts) extras.push(`${node.facts} fact${node.facts === 1 ? "" : "s"}`);
  if (extras.length) lines.push(extras.join(" · "));
  return lines;
}

/* ------------------------------------------------------------------ layout */

/** A small seeded PRNG (the website brain's), so a layout is repeatable. */
export function rng(seed) {
  let s = seed >>> 0;
  return function next() {
    s = (s + 0x6d2b79f5) >>> 0;
    let t = s;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/** A string's stable hash, for seeding a node's start. */
function hash(text) {
  let h = 2166136261;
  for (let i = 0; i < text.length; i += 1) h = Math.imul(h ^ text.charCodeAt(i), 16777619);
  return h >>> 0;
}

/**
 * The brain envelope: two hemispheres side by side (the website brain's
 * proportions) with a cerebellum low at the back. Knowledge — pages, facts,
 * projects — lives in the left hemisphere, sources in the right, so the map
 * reads as "what we know" beside "where it came from"; links pull across.
 */
export const BRAIN = Object.freeze({ side: 0.36, rx: 0.42, ry: 0.56, rz: 0.82, floor: 0.7, cortex: 0.74 });

export function hemisphere(node) {
  return node.kind === "session" || node.kind === "source" || node.kind === "github" ? 1 : -1;
}

/** How far out a point sits in its hemisphere: 1 on the envelope, 0 at
 * its centre (the underside is flatter, as on the website's brain). */
export function envelopeRadius(p, side) {
  const dx = (p.x - side * BRAIN.side) / BRAIN.rx;
  const dy = p.y / (p.y < 0 ? BRAIN.ry * BRAIN.floor : BRAIN.ry);
  const dz = p.z / BRAIN.rz;
  return Math.sqrt(dx * dx + dy * dy + dz * dz);
}

/** Pulls a point back inside its hemisphere's envelope; returns it. */
export function constrain(p, side) {
  const cx = side * BRAIN.side;
  const r = envelopeRadius(p, side);
  if (r > 1) {
    p.x = cx + (p.x - cx) / r;
    p.y /= r;
    p.z /= r;
  }
  // Keep the fissure between the hemispheres open.
  if (side * p.x < 0.05) p.x = side * 0.05;
  return p;
}

/**
 * A 3D force layout over the graph, stepped by the caller (so the page can
 * draw the brain settling): springs along edges, a Barnes–Hut octree for
 * the repulsion (n log n — 2,000 nodes stay interactive), a pull toward each
 * node's hemisphere, and the envelope as a hard limit.
 */
export function createLayout(graph, { seed = 11 } = {}) {
  const n = graph.nodes.length;
  const pos = new Float32Array(n * 3);
  const vel = new Float32Array(n * 3);
  const side = new Int8Array(n);
  graph.nodes.forEach((node, at) => {
    const r = rng(hash(node.id) ^ seed);
    side[at] = hemisphere(node);
    const u = r() * 2 - 1;
    const th = r() * Math.PI * 2;
    const s = Math.sqrt(1 - u * u);
    const rad = 0.3 + 0.6 * Math.cbrt(r());
    pos[at * 3] = side[at] * BRAIN.side + s * Math.cos(th) * BRAIN.rx * rad;
    pos[at * 3 + 1] = u * BRAIN.ry * rad;
    pos[at * 3 + 2] = s * Math.sin(th) * BRAIN.rz * rad;
  });
  const neighbours = graph.edges;
  // The resting length and the repulsion scale with the node count, so a
  // ten-node brain and a two-thousand-node brain both fill the envelope.
  const rest = Math.max(0.035, 0.32 / Math.cbrt(Math.max(1, n)));
  const charge = rest * rest * 0.9;
  let alpha = 1;
  let ticks = 0;

  function repulse() {
    // Octree over the current positions.
    let minX = Infinity, minY = Infinity, minZ = Infinity, maxX = -Infinity, maxY = -Infinity, maxZ = -Infinity;
    for (let i = 0; i < n; i += 1) {
      const x = pos[i * 3], y = pos[i * 3 + 1], z = pos[i * 3 + 2];
      if (x < minX) minX = x; if (x > maxX) maxX = x;
      if (y < minY) minY = y; if (y > maxY) maxY = y;
      if (z < minZ) minZ = z; if (z > maxZ) maxZ = z;
    }
    const size = Math.max(maxX - minX, maxY - minY, maxZ - minZ, 1e-3);
    const root = { x0: minX, y0: minY, z0: minZ, s: size, m: 0, cx: 0, cy: 0, cz: 0, kids: null, body: -1 };
    const insert = (cell, i, depth) => {
      const x = pos[i * 3], y = pos[i * 3 + 1], z = pos[i * 3 + 2];
      cell.cx = (cell.cx * cell.m + x) / (cell.m + 1);
      cell.cy = (cell.cy * cell.m + y) / (cell.m + 1);
      cell.cz = (cell.cz * cell.m + z) / (cell.m + 1);
      cell.m += 1;
      if (!cell.kids && cell.body < 0) {
        cell.body = i;
        return;
      }
      if (depth > 18) return; // coincident points: share the cell
      if (!cell.kids) {
        cell.kids = new Array(8).fill(null);
        const old = cell.body;
        cell.body = -1;
        if (old >= 0) place(cell, old, depth);
      }
      place(cell, i, depth);
    };
    const place = (cell, i, depth) => {
      const h = cell.s / 2;
      const x = pos[i * 3], y = pos[i * 3 + 1], z = pos[i * 3 + 2];
      const ix = x >= cell.x0 + h ? 1 : 0, iy = y >= cell.y0 + h ? 1 : 0, iz = z >= cell.z0 + h ? 1 : 0;
      const slot = ix + iy * 2 + iz * 4;
      if (!cell.kids[slot]) {
        cell.kids[slot] = { x0: cell.x0 + ix * h, y0: cell.y0 + iy * h, z0: cell.z0 + iz * h, s: h, m: 0, cx: 0, cy: 0, cz: 0, kids: null, body: -1 };
      }
      insert(cell.kids[slot], i, depth + 1);
    };
    for (let i = 0; i < n; i += 1) insert(root, i, 0);
    const theta2 = 0.81;
    const stack = [];
    for (let i = 0; i < n; i += 1) {
      const x = pos[i * 3], y = pos[i * 3 + 1], z = pos[i * 3 + 2];
      let fx = 0, fy = 0, fz = 0;
      stack.length = 0;
      stack.push(root);
      while (stack.length) {
        const cell = stack.pop();
        if (!cell.m || cell.body === i) continue;
        const dx = x - cell.cx, dy = y - cell.cy, dz = z - cell.cz;
        const d2 = dx * dx + dy * dy + dz * dz + 1e-6;
        if (!cell.kids || (cell.s * cell.s) / d2 < theta2) {
          const f = (charge * cell.m) / d2;
          const d = Math.sqrt(d2);
          fx += (dx / d) * f; fy += (dy / d) * f; fz += (dz / d) * f;
        } else {
          for (const kid of cell.kids) if (kid) stack.push(kid);
        }
      }
      vel[i * 3] += fx * alpha;
      vel[i * 3 + 1] += fy * alpha;
      vel[i * 3 + 2] += fz * alpha;
    }
  }

  function tick() {
    if (!n) return false;
    repulse();
    for (const edge of neighbours) {
      const a = edge.a * 3, b = edge.b * 3;
      const dx = pos[b] - pos[a], dy = pos[b + 1] - pos[a + 1], dz = pos[b + 2] - pos[a + 2];
      const d = Math.sqrt(dx * dx + dy * dy + dz * dz) + 1e-6;
      const f = ((d - rest) / d) * 0.06 * alpha;
      vel[a] += dx * f; vel[a + 1] += dy * f; vel[a + 2] += dz * f;
      vel[b] -= dx * f; vel[b + 1] -= dy * f; vel[b + 2] -= dz * f;
    }
    const p = { x: 0, y: 0, z: 0 };
    for (let i = 0; i < n; i += 1) {
      const o = i * 3;
      // The cortex: like the website's brain, nodes live in a shell near
      // the surface, so a node sunk toward the middle is pushed outward.
      p.x = pos[o];
      p.y = pos[o + 1];
      p.z = pos[o + 2];
      const r = envelopeRadius(p, side[i]);
      if (r < BRAIN.cortex) {
        const push = ((BRAIN.cortex - r) / Math.max(r, 0.05)) * 0.02;
        vel[o] += (pos[o] - side[i] * BRAIN.side) * push;
        vel[o + 1] += pos[o + 1] * push;
        vel[o + 2] += pos[o + 2] * push;
      }
      vel[o] *= 0.6; vel[o + 1] *= 0.6; vel[o + 2] *= 0.6;
      p.x = pos[o] + vel[o];
      p.y = pos[o + 1] + vel[o + 1];
      p.z = pos[o + 2] + vel[o + 2];
      constrain(p, side[i]);
      pos[o] = p.x; pos[o + 1] = p.y; pos[o + 2] = p.z;
    }
    alpha *= 0.985;
    ticks += 1;
    return alpha > 0.02;
  }

  return {
    positions: pos,
    tick,
    /** Runs up to `count` ticks; true while it is still settling. */
    run(count) {
      let going = true;
      for (let k = 0; k < count && going; k += 1) going = tick();
      return going;
    },
    get ticks() {
      return ticks;
    },
    get alpha() {
      return alpha;
    },
  };
}
