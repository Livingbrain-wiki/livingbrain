// Diagrams generated from the brain graph (and from a page's own structure).
//
// Pure: no DOM, no fetch, no Mermaid. Every function takes the data the app
// already has (`GET /v1/graph`, the page response, its facts) and returns
// Mermaid source plus the link each node should follow. Nothing here is
// written by a model, and nothing is stored: the same inputs always give the
// same source, so a diagram never churns between reads.
//
// Everything that reaches Mermaid is a label built by `sanitizeLabel`, which
// drops the characters Mermaid reads as syntax or markup (quotes, angle
// brackets, brackets, pipes, `#`, `;`, `:`, `%`) and control characters. The
// renderer runs with `securityLevel: "strict"` and `htmlLabels: false`, so a
// label can only ever be text.
//
// `diagram-view.js` (the browser half) loads the vendored Mermaid build and
// draws these. Run the tests with `node --test "app/tests/**/*.test.mjs"`.

/** The vendored Mermaid build, pinned. The SRI hash is the file's sha384. */
export const MERMAID_VERSION = "11.17.2";
export const MERMAID_URL = "assets/vendor/mermaid/mermaid-11.17.2.min.js";
export const MERMAID_SRI =
  "sha384-EOXBFmc3gx5mb+vn0vPvvGqACToJD24hhacX5Yx+8NUUQrHIle/Qi5Bg9o3zKwW2";

export const MAX_NEIGHBOURS = 12;
export const MAX_STEPS = 12;
export const MAX_EVENTS = 20;
export const MIN_EVENTS = 3;
export const MIN_NEIGHBOURS = 2;
export const MIN_STEPS = 3;
/** At most this many diagrams are generated for one page. */
export const MAX_PAGE_DIAGRAMS = 2;

/** Entity pages that get a relationship map. */
export const ENTITY_TYPES = new Set(["person", "project", "customer", "system", "decision", "glossary"]);

/** Node kinds in the order their groups appear; anything else is "Other". */
const GROUPS = [
  ["person", "People"],
  ["project", "Projects"],
  ["customer", "Customers"],
  ["system", "Systems"],
  ["decision", "Decisions"],
  ["glossary", "Glossary"],
  ["topic", "Topics"],
  ["coding-agents", "Compiled pages"],
  ["session", "Sessions"],
  ["radar", "Radar"],
  ["source", "Sources"],
  ["fact", "Facts"],
  ["other", "Other"],
];
const GROUP_LABEL = new Map(GROUPS);
const GROUP_RANK = new Map(GROUPS.map(([kind], index) => [kind, index]));

/* ------------------------------------------------------------ colour */

const FALLBACK_HEX = "#888888";

/**
 * `oklch(L C H)` (or `oklch(L C H / a)`, alpha ignored) to `#rrggbb`, clipped
 * to the sRGB gamut. A `#rgb`/`#rrggbb` value passes through. Anything else
 * gives `null`, so the caller can use its own fallback.
 */
export function oklchToHex(value) {
  const text = String(value || "").trim();
  if (/^#[0-9a-f]{6}$/i.test(text)) return text.toLowerCase();
  const match = /^oklch\(\s*([\d.]+%?)\s+([\d.]+)\s+([\d.]+)(?:\s*\/\s*[\d.]+%?)?\s*\)$/i.exec(text);
  if (!match) return null;
  const lightness = match[1].endsWith("%") ? Number.parseFloat(match[1]) / 100 : Number.parseFloat(match[1]);
  const chroma = Number.parseFloat(match[2]);
  const hue = (Number.parseFloat(match[3]) * Math.PI) / 180;
  const a = chroma * Math.cos(hue);
  const b = chroma * Math.sin(hue);
  const l = (lightness + 0.3963377774 * a + 0.2158037573 * b) ** 3;
  const m = (lightness - 0.1055613458 * a - 0.0638541728 * b) ** 3;
  const s = (lightness - 0.0894841775 * a - 1.291485548 * b) ** 3;
  const linear = [
    4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
    -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
    -0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s,
  ];
  const hex = linear.map((channel) => {
    const clipped = Math.min(1, Math.max(0, channel));
    const encoded = clipped <= 0.0031308 ? 12.92 * clipped : 1.055 * clipped ** (1 / 2.4) - 0.055;
    return Math.round(Math.min(1, Math.max(0, encoded)) * 255)
      .toString(16)
      .padStart(2, "0");
  });
  return `#${hex.join("")}`;
}

/**
 * The palette the diagrams use, read from the app's design tokens. `tokens`
 * maps a custom-property name (`--bg2`) to its value as written in CSS. A
 * missing or unparseable token takes the dark theme's value, so a diagram is
 * always drawn in colours that exist in the app.
 */
const PALETTE_TOKENS = {
  background: ["--bg", "#0b1517"],
  surface: ["--bg2", "#14292c"],
  surfaceSunken: ["--bg3", "#1d3a3e"],
  ink: ["--ink", "#eef4f5"],
  ink2: ["--ink2", "#c2d0d2"],
  line: ["--line", "#4a6266"],
  line2: ["--line2", "#2c4447"],
  accent: ["--accent", "#6ef0b4"],
  accentInk: ["--accent-ink", "#06281b"],
  edge: ["--graph-edge", "#8fa7a9"],
};

export function palette(tokens = {}) {
  const out = {};
  for (const [key, [name, fallback]] of Object.entries(PALETTE_TOKENS)) {
    out[key] = oklchToHex(tokens[name]) || fallback;
  }
  return out;
}

/* ------------------------------------------------------------ labels */

const UNSAFE = /[\\"<>{}[\]()|#;:`%$&*'~^=]/g;
const CONTROL = /[\u0000-\u001f\u007f-\u009f]/g;

/**
 * A label Mermaid can show as plain text: no syntax characters, no control
 * characters, collapsed spaces, at most `max` characters. Never empty.
 */
export function sanitizeLabel(value, max = 48) {
  const text = String(value ?? "")
    .normalize("NFC")
    .replace(CONTROL, " ")
    .replace(UNSAFE, " ")
    .replace(/\s+/g, " ")
    .trim();
  if (!text) return "untitled";
  return text.length > max ? `${text.slice(0, max - 1).trimEnd()}…` : text;
}

/** A lowercase ASCII slug, `fallback` when nothing is left. */
export function slugify(value, fallback = "diagram", max = 80) {
  const slug = String(value ?? "")
    .normalize("NFKD")
    .replace(/[̀-ͯ]/g, "")
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, max)
    .replace(/-+$/g, "");
  return slug || fallback;
}

/** `relationship-map`, `flow`, `timeline`: the diagram's kind as a file-name part. */
export function diagramFilename({ title, type, ext }) {
  return `${slugify(title, "brain")}-${slugify(type, "diagram")}.${ext}`;
}

/* ------------------------------------------------------------ theme */

/** The twelve palette slots Mermaid indexes, alternating the surface pair. */
function scaleVariables(colours) {
  const out = {};
  for (let i = 0; i < 12; i += 1) {
    out[`cScale${i}`] = i % 2 ? colours.surfaceSunken : colours.surface;
    out[`cScaleLabel${i}`] = colours.ink;
    out[`cScaleInv${i}`] = colours.surfaceSunken;
  }
  return out;
}

/** The options `mermaid.initialize` takes for one palette. */
export function mermaidConfig(colours, font = "Hanken Grotesk") {
  return {
    startOnLoad: false,
    securityLevel: "strict",
    theme: "base",
    htmlLabels: false,
    fontFamily: font,
    themeVariables: {
      background: colours.background,
      fontFamily: font,
      fontSize: "14px",
      primaryColor: colours.surface,
      primaryTextColor: colours.ink,
      primaryBorderColor: colours.line,
      secondaryColor: colours.surfaceSunken,
      tertiaryColor: colours.surfaceSunken,
      mainBkg: colours.surface,
      nodeBorder: colours.line,
      nodeTextColor: colours.ink,
      lineColor: colours.edge,
      defaultLinkColor: colours.edge,
      textColor: colours.ink2,
      titleColor: colours.ink,
      clusterBkg: colours.surfaceSunken,
      clusterBorder: colours.line2,
      edgeLabelBackground: colours.background,
      // Timeline sections and any other palette-indexed part take the calm
      // surface pair, never Mermaid's default hues.
      ...scaleVariables(colours),
      sectionBkgColor: colours.surfaceSunken,
      sectionBkgColor2: colours.surface,
      altSectionBkgColor: colours.surface,
      sectionTextColor: colours.ink,
      taskBkgColor: colours.surface,
      taskBorderColor: colours.line,
      taskTextColor: colours.ink,
      activeTaskBkgColor: colours.surfaceSunken,
      activeTaskBorderColor: colours.line,
      gridColor: colours.line2,
    },
    flowchart: { htmlLabels: false, curve: "basis", padding: 14, nodeSpacing: 34, rankSpacing: 44, useMaxWidth: false },
    timeline: { useMaxWidth: false, padding: 12 },
  };
}

/* ------------------------------------------------------------ graph */

/** Groups a graph node kind for display. */
export function groupOf(kind) {
  return GROUP_LABEL.has(kind) ? kind : "other";
}

/** The group heading a kind is drawn under. */
export function groupLabel(kind) {
  return GROUP_LABEL.get(groupOf(kind));
}

function strengthOf(edges, focusId) {
  const counts = new Map();
  for (const edge of edges) {
    const other = edge.source === focusId ? edge.target : edge.target === focusId ? edge.source : null;
    if (other == null) continue;
    counts.set(other, (counts.get(other) || 0) + 1);
  }
  return counts;
}

/**
 * The focus entity in the centre and up to `max` of its strongest neighbours,
 * grouped by kind. Strength is how many edges join the two; ties go to the
 * better-connected node, then to the label, then to the id, so the order is
 * stable. Returns `null` when there are fewer than `MIN_NEIGHBOURS`
 * neighbours, so a page never gets an empty or one-node map.
 */
export function relationshipMap({ focusId, nodes, edges, max = MAX_NEIGHBOURS, colours }) {
  const byId = new Map((nodes || []).map((node) => [node.id, node]));
  const focus = byId.get(focusId);
  if (!focus) return null;
  const seen = new Set();
  const uniqueEdges = [];
  for (const edge of edges || []) {
    if (!byId.has(edge.source) || !byId.has(edge.target) || edge.source === edge.target) continue;
    const key = `${edge.source}\u0000${edge.target}\u0000${edge.kind}`;
    if (seen.has(key)) continue;
    seen.add(key);
    uniqueEdges.push(edge);
  }
  const strength = strengthOf(uniqueEdges, focusId);
  const ranked = [...strength.keys()]
    .map((id) => byId.get(id))
    .sort(
      (a, b) =>
        strength.get(b.id) - strength.get(a.id) ||
        (b.degree || 0) - (a.degree || 0) ||
        sanitizeLabel(a.label).localeCompare(sanitizeLabel(b.label)) ||
        (a.id < b.id ? -1 : a.id > b.id ? 1 : 0),
    )
    .slice(0, max);
  if (ranked.length < MIN_NEIGHBOURS) return null;

  const chosen = new Set(ranked.map((node) => node.id));
  // Node ids are generated (n0, n1, ...), never taken from content.
  const name = new Map();
  name.set(focusId, "n0");
  ranked.forEach((node, index) => name.set(node.id, `n${index + 1}`));


  const groups = new Map();
  for (const node of ranked) {
    const group = groupOf(node.kind);
    if (!groups.has(group)) groups.set(group, []);
    groups.get(group).push(node);
  }
  const orderedGroups = [...groups.keys()].sort((a, b) => GROUP_RANK.get(a) - GROUP_RANK.get(b));

  const lines = ["flowchart LR", `  ${name.get(focusId)}(["${sanitizeLabel(focus.label)}"])`];
  const links = {};
  const groupIndex = [];
  orderedGroups.forEach((group, index) => {
    const members = groups.get(group);
    const gid = `g${index}`;
    groupIndex.push(gid);
    lines.push(`  subgraph ${gid}["${GROUP_LABEL.get(group)}"]`);
    for (const node of members) {
      const id = name.get(node.id);
      lines.push(`    ${id}["${sanitizeLabel(node.label)}"]`);
      if (node.slug) links[id] = node.slug;
    }
    lines.push("  end");
  });
  for (const node of ranked) {
    const id = name.get(node.id);
    // Every arrow leaves the focus: one direction keeps the map a fan, with no crossings.
    lines.push(`  ${name.get(focusId)} --> ${id}`);
  }
  // The focus node is painted after render (diagram-view.js), not with a
  // Mermaid style directive: a `style` line here also reached a subgraph.
  lines.push(`  linkStyle default stroke:${colours ? colours.edge : "#8fa7a9"},stroke-width:1.2px`);
  // The same neighbours as a list, for keyboard users and anyone without the picture.
  const items = ranked
    .filter((node) => node.slug)
    .map((node) => ({ label: sanitizeLabel(node.label), slug: node.slug, group: groupLabel(node.kind) }));
  const highlight = colours
    ? { id: name.get(focusId), fill: colours.accent, ink: colours.accentInk, line: colours.line2, clusterInk: colours.ink2 }
    : null;
  return { type: "relationship", source: lines.join("\n"), links, items, highlight, count: ranked.length, groups: groupIndex.length };
}

/* ------------------------------------------------------------ steps */

/** Plain text of one Markdown list item: links, emphasis and code marks removed. */
function plainStep(text) {
  return String(text || "")
    .replace(/\[([^\]]*)\]\([^)]*\)/g, "$1")
    .replace(/[*_`]+/g, "")
    .replace(/\s+/g, " ")
    .trim();
}

/**
 * The first run of at least `MIN_STEPS` top-level ordered-list items in a
 * page, outside code fences, as plain step text (at most `MAX_STEPS`).
 */
export function stepsFromMarkdown(markdown) {
  const lines = String(markdown || "").split(/\r?\n/);
  let inFence = false;
  let run = [];
  const runs = [];
  for (const line of lines) {
    if (/^\s*```/.test(line)) {
      inFence = !inFence;
      if (run.length) runs.push(run);
      run = [];
      continue;
    }
    if (inFence) continue;
    const item = /^\d{1,3}[.)]\s+(\S.*)$/.exec(line);
    if (item) {
      run.push(plainStep(item[1]));
    } else {
      if (run.length) runs.push(run);
      run = [];
    }
  }
  if (run.length) runs.push(run);
  const best = runs.find((steps) => steps.length >= MIN_STEPS);
  return best ? best.slice(0, MAX_STEPS) : [];
}

/** A top-to-bottom flowchart of a page's steps, or `null` below `MIN_STEPS`. */
export function flowchartFromSteps(steps) {
  const list = (steps || []).filter((step) => plainStep(step));
  if (list.length < MIN_STEPS) return null;
  const lines = ["flowchart TD"];
  list.forEach((step, index) => lines.push(`  s${index}["${sanitizeLabel(step, 60)}"]`));
  for (let index = 1; index < list.length; index += 1) lines.push(`  s${index - 1} --> s${index}`);
  return { type: "flow", source: lines.join("\n"), links: {}, count: list.length };
}

/* ------------------------------------------------------------ timeline */

const DATE = /^\d{4}(?:-\d{2}(?:-\d{2})?)?$/;

/** The day a fact is dated: when it became true, else when it was observed. */
function factDate(fact) {
  const from = fact.valid_from || String(fact.observed_at || "").slice(0, 10);
  return DATE.test(from) ? from : null;
}

/**
 * A Mermaid timeline of dated facts: one event per fact, sections by year,
 * the newest `MAX_EVENTS` in chronological order. Needs `MIN_EVENTS` distinct
 * dated events, else `null`.
 */
export function timelineFromFacts(facts, { title = "Timeline" } = {}) {
  const seen = new Set();
  const events = [];
  for (const fact of Array.isArray(facts) ? facts : []) {
    if (!fact || !fact.attribute) continue;
    const date = factDate(fact);
    if (!date) continue;
    const value = [fact.value, fact.unit].filter((part) => part != null && String(part) !== "").join(" ");
    const label = sanitizeLabel(`${fact.attribute} ${value}`.trim(), 60);
    const key = `${date}\u0000${label}`;
    if (seen.has(key)) continue;
    seen.add(key);
    events.push({ date, label });
  }
  if (events.length < MIN_EVENTS) return null;
  events.sort((a, b) => (a.date < b.date ? -1 : a.date > b.date ? 1 : a.label.localeCompare(b.label)));
  const shown = events.slice(-MAX_EVENTS);
  const lines = ["timeline", `  title ${sanitizeLabel(title, 60)}`];
  let year = null;
  for (const event of shown) {
    const y = event.date.slice(0, 4);
    if (y !== year) {
      year = y;
      lines.push(`  section ${y}`);
    }
    lines.push(`    ${event.date} : ${event.label}`);
  }
  // The drawn timeline is an SVG (timelineSvg); the Mermaid source is what "Copy Mermaid source" gives.
  const drawn = shown.map((event) => ({ date: event.date, short: shortDate(event.date), text: event.label }));
  return { type: "timeline", source: lines.join("\n"), events: drawn, links: {}, count: shown.length };
}

const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/** `1842`, `Jul 1842` or `15 Mar 2024`, from a `YYYY`, `YYYY-MM` or `YYYY-MM-DD` date. */
export function shortDate(date) {
  const [year, month, day] = String(date).split("-");
  if (!month) return year;
  const name = MONTHS[Number(month) - 1] || "";
  return day ? `${Number(day)} ${name} ${year}` : `${name} ${year}`;
}

/** Word-wraps `text` to `width` characters over at most `maxLines` lines, ending the last with an ellipsis if cut. */
export function wrapText(text, width = 17, maxLines = 3) {
  const words = String(text || "").split(" ").filter(Boolean);
  const lines = [];
  let current = "";
  for (const word of words) {
    const next = current ? `${current} ${word}` : word;
    if (next.length <= width) {
      current = next;
      continue;
    }
    if (current) lines.push(current);
    current = word.length > width ? `${word.slice(0, width - 1)}…` : word;
  }
  if (current) lines.push(current);
  if (lines.length <= maxLines) return lines;
  const kept = lines.slice(0, maxLines);
  const last = kept[maxLines - 1];
  kept[maxLines - 1] = `${last.slice(0, Math.max(0, width - 1)).trimEnd()}…`;
  return kept;
}

const SVG_FONT = "Hanken Grotesk, system-ui, sans-serif";

/** The timeline drawn as one row: a date above each dot on a clean axis, the event below, wrapped. */
export function timelineSvg(events, colours) {
  const list = Array.isArray(events) ? events : [];
  if (!list.length || !colours) return null;
  const column = 130;
  const axis = 78;
  const height = 158;
  const width = list.length * column;
  const parts = [
    `<line x1="0" y1="${axis}" x2="${width}" y2="${axis}" stroke="${colours.line}" stroke-width="2" stroke-linecap="round"/>`,
  ];
  list.forEach((event, index) => {
    const cx = index * column + column / 2;
    parts.push(
      `<text x="${cx}" y="40" text-anchor="middle" font-family="${SVG_FONT}" font-size="12" font-weight="600" fill="${colours.ink}">${xmlText(event.short)}</text>`,
      `<circle cx="${cx}" cy="${axis}" r="6" fill="${colours.accent}" stroke="${colours.background}" stroke-width="2"/>`,
    );
    wrapText(event.text, 17, 3).forEach((line, row) => {
      parts.push(
        `<text x="${cx}" y="${axis + 32 + row * 18}" text-anchor="middle" font-family="${SVG_FONT}" font-size="13" fill="${colours.ink2}">${xmlText(line)}</text>`,
      );
    });
  });
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${width} ${height}" width="${width}" height="${height}" role="img" class="lbd-timeline">${parts.join("")}</svg>`;
}

function xmlText(value) {
  return String(value).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

/* ------------------------------------------------------------ page */

/**
 * The diagrams one page gets, in order: relationship map (entity pages), flow
 * (system and process pages with steps), timeline (dated facts). At most
 * `MAX_PAGE_DIAGRAMS`, and each is skipped when its data is too thin.
 *
 * `graph` is `{nodes, edges}` from `GET /v1/graph` (or null); `focusId` is the
 * page's node id in it. `facts` is a flat list of facts (current and history).
 */
export function diagramsForPage({ entityType, title, markdown, graph, focusId, facts, colours }) {
  const out = [];
  if (ENTITY_TYPES.has(entityType) && graph && focusId) {
    const map = relationshipMap({ focusId, nodes: graph.nodes, edges: graph.edges, colours });
    if (map) out.push({ ...map, label: "Relationship map" });
  }
  if (entityType === "system" || /\b(deploy|deployment|architecture|flow|pipeline|runbook|process|release)\b/i.test(String(title || ""))) {
    const flow = flowchartFromSteps(stepsFromMarkdown(markdown));
    if (flow) out.push({ ...flow, label: "Flow" });
  }
  const timeline = timelineFromFacts(facts, { title: title || "Timeline" });
  if (timeline) {
    const items = timeline.events.map((event) => ({ label: `${event.short}: ${event.text}`, group: "" }));
    out.push({ ...timeline, label: "Timeline", items, svg: timelineSvg(timeline.events, colours) });
  }
  return out.slice(0, MAX_PAGE_DIAGRAMS);
}

/** The facts of a page's `facts` keys, current and history, as one flat list. */
export function flattenFacts(keys) {
  const out = [];
  for (const key of Array.isArray(keys) ? keys : []) {
    if (!key) continue;
    for (const fact of [...(key.current || []), ...(key.history || [])]) {
      if (fact) out.push({ ...fact, attribute: fact.attribute || key.attribute });
    }
  }
  return out;
}

/* ------------------------------------------------------------ export */

/** A `@font-face` rule with the font inlined as a data URI. */
export function fontFaceRule(family, base64, weight = "400 700") {
  return `@font-face{font-family:"${family}";font-style:normal;font-weight:${weight};src:url(data:font/woff2;base64,${base64}) format("woff2");}`;
}

/**
 * An exportable SVG document from a rendered Mermaid SVG string: the diagram
 * padded by `pad` on every side, filled with `background`, its styles and
 * the inlined `fontCss` inside the file so it looks the same anywhere. Returns
 * `null` when the text has no SVG root with a usable viewBox.
 */
export function serializeSvg(svgText, { pad = 24, background = "#ffffff", fontCss = "" } = {}) {
  const text = String(svgText || "");
  const open = /<svg\b[^>]*>/i.exec(text);
  if (!open) return null;
  const attrs = {};
  for (const m of open[0].matchAll(/([\w:-]+)\s*=\s*"([^"]*)"/g)) attrs[m[1]] = m[2];
  const box = (attrs.viewBox || "").trim().split(/[\s,]+/).map(Number);
  if (box.length !== 4 || box.some((n) => !Number.isFinite(n)) || box[2] <= 0 || box[3] <= 0) return null;
  const [x, y, w, h] = box;
  const width = w + pad * 2;
  const height = h + pad * 2;
  const keep = Object.entries(attrs).filter(([name]) => !["width", "height", "style", "viewBox", "xmlns"].includes(name));
  const tagAttrs = [
    `xmlns="http://www.w3.org/2000/svg"`,
    `viewBox="${x - pad} ${y - pad} ${width} ${height}"`,
    `width="${width}"`,
    `height="${height}"`,
    ...keep.map(([name, value]) => `${name}="${value}"`),
  ].join(" ");
  const rest = text.slice(open.index + open[0].length).replace(/^[\s]*/, "");
  const style = fontCss ? `<style>${fontCss}</style>` : "";
  const bg = `<rect x="${x - pad}" y="${y - pad}" width="${width}" height="${height}" fill="${background}"/>`;
  const body = rest.replace(/<\/svg>\s*$/i, "");
  return `<?xml version="1.0" encoding="UTF-8"?>\n<svg ${tagAttrs}>${style}${bg}${body}</svg>\n`;
}
