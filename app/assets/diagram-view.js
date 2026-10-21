// The browser half of the diagrams: loads the vendored Mermaid build on the
// first diagram of a page, draws each source with the app's own colours, and
// wraps it in a card with an export menu and a fullscreen view.
//
// Two kinds of diagram reach here: a ```mermaid fence in a page (the figure
// `markdown.js` leaves, with its code block as the fallback), and the maps
// `diagrams.js` generates from the graph and the page's facts. Both go through
// `drawSource`, which Mermaid runs with `securityLevel: "strict"` and no HTML
// labels. The SVG it returns is parsed as SVG and imported; nothing is put
// into the page as an HTML string.

import {
  ENTITY_TYPES,
  MERMAID_SRI,
  MERMAID_URL,
  diagramFilename,
  diagramsForPage,
  fontFaceRule,
  flattenFacts,
  mermaidConfig,
  palette,
  serializeSvg,
} from "./diagrams.js";

const FONT_FAMILY = "Hanken Grotesk";
const FONT_URL = "assets/fonts/hanken-grotesk.woff2";
const EXPORT_PAD = 24;
const GRAPH_TTL = 60000;
const TOKENS = ["--bg", "--bg2", "--bg3", "--ink", "--ink2", "--line", "--line2", "--accent", "--accent-ink", "--graph-edge"];

/** Every card on the page, so a theme change can redraw them. */
const cards = new Set();
let listening = false;
let loading = null;
let chain = Promise.resolve();
let counter = 0;
let graphMemo = null;

/* ------------------------------------------------------------ dom */

function h(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [name, value] of Object.entries(attrs)) {
    if (value === false || value == null) continue;
    node.setAttribute(name, value === true ? "" : String(value));
  }
  for (const child of children.flat()) {
    if (child == null || child === false) continue;
    node.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return node;
}

/* ------------------------------------------------------------ mermaid */

/** Loads the pinned, vendored Mermaid build once, with its SRI hash. */
export function loadMermaid() {
  if (globalThis.mermaid) return Promise.resolve(globalThis.mermaid);
  if (loading) return loading;
  loading = new Promise((resolve, reject) => {
    const script = h("script", { src: MERMAID_URL, integrity: MERMAID_SRI, async: true });
    script.addEventListener("load", () => {
      if (globalThis.mermaid) resolve(globalThis.mermaid);
      else reject(new Error("mermaid did not register"));
    });
    script.addEventListener("error", () => {
      script.remove();
      loading = null;
      reject(new Error("mermaid failed to load"));
    });
    document.head.append(script);
  });
  return loading;
}

/** The app's colours as the diagrams use them, read live from the tokens. */
export function currentPalette() {
  const style = getComputedStyle(document.documentElement);
  const tokens = {};
  for (const name of TOKENS) tokens[name] = style.getPropertyValue(name).trim();
  return palette(tokens);
}

/** Renders are serial: Mermaid's configuration is global, so one at a time. */
function enqueue(task) {
  const run = chain.then(task, task);
  chain = run.catch(() => {});
  return run;
}

async function drawSource(source, colours) {
  const mermaid = await loadMermaid();
  const id = `lbd-${++counter}`;
  return enqueue(async () => {
    mermaid.initialize(mermaidConfig(colours, FONT_FAMILY));
    try {
      const { svg } = await mermaid.render(id, source);
      return svg;
    } catch (error) {
      document.getElementById(`d${id}`)?.remove();
      throw error;
    }
  });
}

/*
 * The page's Content-Security-Policy has `style-src 'self'` with no
 * 'unsafe-inline', so Mermaid's own output cannot be written into the page as
 * markup: its `style="…"` attributes and its `<style>` block are refused there.
 * They are moved instead, and applied through the CSS object model, which the
 * policy does not govern: each `style` attribute goes to `element.style`, and
 * the `<style>` text becomes a constructable stylesheet adopted by the document.
 * The rules are scoped by the diagram's own `#lbd-N` id, so diagrams do not
 * clash. The SVG text itself is kept whole for the exports.
 */
function svgNode(svgText) {
  const doc = new DOMParser().parseFromString(svgText, "image/svg+xml");
  const root = doc.documentElement;
  if (!root || root.localName !== "svg" || doc.getElementsByTagName("parsererror").length) {
    throw new Error("not an svg");
  }
  const css = [...root.getElementsByTagName("style")].map((el) => el.textContent).join("\n");
  for (const el of [...root.getElementsByTagName("style")]) el.remove();
  const inline = [];
  for (const el of [root, ...root.querySelectorAll("[style]")]) {
    if (el.hasAttribute("style")) {
      inline.push(el.getAttribute("style"));
      el.removeAttribute("style");
    } else {
      inline.push(null);
    }
  }
  const node = document.importNode(root, true);
  const targets = [node, ...node.querySelectorAll("*")].filter((el) => el.namespaceURI === root.namespaceURI);
  let index = 0;
  for (const el of targets) {
    if (index < inline.length && inline[index] != null) el.style.cssText = inline[index];
    index += 1;
  }
  return { node, css };
}

/** Adopts a card's diagram stylesheet, replacing the one it had before. */
function adoptSheet(card, css) {
  if (!css || typeof CSSStyleSheet === "undefined" || !("adoptedStyleSheets" in document)) return;
  const sheet = new CSSStyleSheet();
  sheet.replaceSync(css);
  dropSheet(card);
  document.adoptedStyleSheets = [...document.adoptedStyleSheets, sheet];
  card.sheet = sheet;
}

function dropSheet(card) {
  if (!card.sheet) return;
  document.adoptedStyleSheets = document.adoptedStyleSheets.filter((sheet) => sheet !== card.sheet);
  card.sheet = null;
}

/** Node ids are `n0`, `n1`, ... and Mermaid prefixes them, so match the suffix. */
function bindLinks(svg, links, go) {
  for (const group of svg.querySelectorAll("g.node")) {
    const match = /-(n\d+)-\d+$/.exec(group.id || "");
    const slug = match && links[match[1]];
    if (!slug) continue;
    group.classList.add("is-link");
    group.addEventListener("click", () => go(slug));
  }
}

/** Paints the focus node of a relationship map with the accent, through the CSSOM. */
function highlightNode(svg, highlight) {
  if (!highlight) return;
  const pattern = new RegExp(`-${highlight.id}-\\d+$`);
  const group = [...svg.querySelectorAll("g.node")].find((g) => pattern.test(g.id || ""));
  if (!group) return;
  for (const shape of group.querySelectorAll("rect, polygon, path, circle, ellipse")) {
    shape.style.setProperty("fill", highlight.fill);
    shape.style.setProperty("stroke", highlight.fill);
  }
  for (const label of group.querySelectorAll(".nodeLabel, text, span, p")) {
    label.style.setProperty("color", highlight.ink);
    label.style.setProperty("fill", highlight.ink);
  }
}

/* ------------------------------------------------------------ export */

let fontPromise = null;

function toBase64(bytes) {
  let out = "";
  for (let i = 0; i < bytes.length; i += 0x8000) {
    out += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return btoa(out);
}

/** Hanken Grotesk inlined as a data URI, so an export draws the same font. */
function exportFont() {
  if (!fontPromise) {
    fontPromise = fetch(FONT_URL)
      .then((response) => {
        if (!response.ok) throw new Error("font");
        return response.arrayBuffer();
      })
      .then((buffer) => fontFaceRule(FONT_FAMILY, toBase64(new Uint8Array(buffer))))
      .catch(() => "");
  }
  return fontPromise;
}
function escapeXml(text) {
  return String(text).replace(/&/g, "&amp;").replace(/</g, "&lt;");
}

/**
 * The card's drawing as it is on screen: the live SVG (so the focus highlight
 * and the link bindings are in it), with the diagram's stylesheet put back as
 * a `<style>` element, because the export has to carry its own styling.
 */
function liveSvgText(card) {
  const live = card.stage.querySelector("svg");
  if (!live) return card.svgText;
  const text = new XMLSerializer().serializeToString(live);
  return text.replace(/^<svg\b[^>]*>/, (open) => `${open}<style>${escapeXml(card.css || "")}</style>`);
}

/** The export SVG document for a card: padded, filled, with fonts and styles. */
async function exportSvg(card) {
  const svg = serializeSvg(liveSvgText(card), {
    pad: EXPORT_PAD,
    background: card.colours.background,
    fontCss: await exportFont(),
  });
  if (!svg) throw new Error("no svg to export");
  return svg;
}

function base64Utf8(text) {
  return toBase64(new TextEncoder().encode(text));
}

/** A 2x PNG of the export SVG, background included. */
async function pngBlob(card, scale = 2) {
  const svg = await exportSvg(card);
  const size = /<svg[^>]*\swidth="([\d.]+)"[^>]*\sheight="([\d.]+)"/.exec(svg);
  if (!size) throw new Error("no size");
  const image = new Image();
  image.src = `data:image/svg+xml;base64,${base64Utf8(svg)}`;
  await image.decode();
  const canvas = h("canvas", {});
  canvas.width = Math.round(Number(size[1]) * scale);
  canvas.height = Math.round(Number(size[2]) * scale);
  canvas.getContext("2d").drawImage(image, 0, 0, canvas.width, canvas.height);
  return new Promise((resolve, reject) => {
    canvas.toBlob((blob) => (blob ? resolve(blob) : reject(new Error("png"))), "image/png");
  });
}

function download(blob, filename) {
  const url = URL.createObjectURL(blob);
  const link = h("a", { href: url, download: filename });
  document.body.append(link);
  link.click();
  link.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

/** The export actions a card's menu offers. Each reports to `say`. */
function exportActions(card, say) {
  const name = (ext) => diagramFilename({ title: card.title, type: card.spec.label, ext });
  return [
    {
      label: "Copy as image",
      run: async () => {
        if (!navigator.clipboard || typeof ClipboardItem === "undefined") {
          say("Copying images is not supported in this browser. Use Download PNG.");
          return;
        }
        // The blob is a promise so the copy keeps the click's user activation.
        await navigator.clipboard.write([new ClipboardItem({ "image/png": pngBlob(card) })]);
        say("Copied");
      },
    },
    {
      label: "Copy Mermaid source",
      run: async () => {
        await navigator.clipboard.writeText(card.spec.source);
        say("Copied");
      },
    },
    {
      label: "Download SVG",
      run: async () => {
        download(new Blob([await exportSvg(card)], { type: "image/svg+xml" }), name("svg"));
      },
    },
    {
      label: "Download PNG",
      run: async () => download(await pngBlob(card), name("png")),
    },
  ];
}

/**
 * The export menu: a button that opens a list of the actions. Arrow keys,
 * Home and End move through it, Escape and Tab close it and return focus to
 * the button, and each action's result is announced in a live region.
 */
function exportMenu(card) {
  const toast = h("p", { class: "diagram-toast", role: "status" });
  let timer = null;
  const say = (message) => {
    toast.textContent = message;
    toast.hidden = false;
    clearTimeout(timer);
    timer = setTimeout(() => {
      toast.hidden = true;
    }, 1800);
  };
  const trigger = h("button", {
    type: "button",
    class: "diagram-btn",
    "aria-haspopup": "menu",
    "aria-expanded": "false",
    "aria-label": `Export ${card.spec.label.toLowerCase()}`,
  }, "Export");
  const list = h("ul", { class: "diagram-menu", role: "menu", hidden: true });
  const items = exportActions(card, say).map((action) => {
    const item = h("button", { type: "button", role: "menuitem", class: "diagram-menu__item" }, action.label);
    item.addEventListener("click", async () => {
      close(false);
      try {
        await action.run();
      } catch {
        say("Could not export. Try Download PNG.");
      }
    });
    list.append(h("li", { role: "none" }, item));
    return item;
  });
  const isOpen = () => !list.hidden;
  function close(returnFocus = true) {
    if (!isOpen()) return;
    list.hidden = true;
    trigger.setAttribute("aria-expanded", "false");
    document.removeEventListener("pointerdown", outside, true);
    if (returnFocus) trigger.focus();
  }
  function open(focusFirst = true) {
    list.hidden = false;
    trigger.setAttribute("aria-expanded", "true");
    document.addEventListener("pointerdown", outside, true);
    if (focusFirst) items[0].focus();
  }
  function outside(event) {
    if (!wrap.contains(event.target)) close(false);
  }
  trigger.addEventListener("click", () => (isOpen() ? close() : open()));
  trigger.addEventListener("keydown", (event) => {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      open();
    }
  });
  list.addEventListener("keydown", (event) => {
    const index = items.indexOf(document.activeElement);
    if (event.key === "Escape") {
      event.preventDefault();
      close();
    } else if (event.key === "Tab") {
      close(false);
    } else if (event.key === "ArrowDown") {
      event.preventDefault();
      items[(index + 1) % items.length].focus();
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      items[(index - 1 + items.length) % items.length].focus();
    } else if (event.key === "Home") {
      event.preventDefault();
      items[0].focus();
    } else if (event.key === "End") {
      event.preventDefault();
      items[items.length - 1].focus();
    }
  });
  const wrap = h("div", { class: "diagram-menu-wrap" }, trigger, list, toast);
  toast.hidden = true;
  return wrap;
}

/* ------------------------------------------------------------ cards */

function listLinks(card) {
  const items = card.spec.items || [];
  if (!items.length) return null;
  return h(
    "details",
    { class: "diagram-card__list" },
    h("summary", {}, `Nodes (${items.length})`),
    h(
      "ul",
      {},
      items.map((item) =>
        h("li", {}, h("a", { href: `wiki.html?slug=${encodeURIComponent(item.slug)}` }, item.label), ` · ${item.group}`),
      ),
    ),
  );
}

/** Replaces the card's stage with a fresh drawing of its current source. */
async function draw(card) {
  if (card.regen) card.spec = card.regen() || card.spec;
  if (!card.spec) return;
  const colours = currentPalette();
  try {
    const svgText = await drawSource(card.spec.source, colours);
    const { node: svg, css } = svgNode(svgText);
    card.svgText = svgText;
    card.css = css;
    card.colours = colours;
    adoptSheet(card, css);
    card.stage.replaceChildren(svg);
    highlightNode(svg, card.spec.highlight);
    bindLinks(svg, card.spec.links || {}, (slug) => {
      window.location.assign(`wiki.html?slug=${encodeURIComponent(slug)}`);
    });
    card.stage.classList.remove("is-failed");
  } catch {
    showFallback(card);
  }
}

/** A failed render shows the source as code, the same as a plain fence. */
function showFallback(card) {
  if (card.fallback) {
    card.figure.replaceWith(card.fallback);
    card.figure = card.fallback;
    cards.delete(card);
    return;
  }
  card.stage.replaceChildren(h("pre", { class: "diagram-card__code" }, h("code", {}, card.spec.source)));
  card.stage.classList.add("is-failed");
}

function openFullscreen(card, trigger) {
  const dialog = h("dialog", { class: "diagram-dialog", "aria-label": `${card.spec.label}, full screen` });
  const close = h("button", { type: "button", class: "diagram-btn" }, "Close");
  const menu = exportMenu(card);
  const stage = h("div", { class: "diagram-dialog__stage", tabindex: "0" });
  if (card.svgText) stage.append(svgNode(card.svgText).node);
  const head = h(
    "header",
    { class: "diagram-card__head" },
    h("span", { class: "diagram-card__kind" }, card.spec.label),
    h("span", { class: "diagram-card__note" }, "generated from the graph"),
    h("span", { class: "diagram-card__tools" }, menu, close),
  );
  dialog.append(head, stage);
  close.addEventListener("click", () => dialog.close());
  dialog.addEventListener("close", () => {
    dialog.remove();
    trigger?.focus();
  });
  document.body.append(dialog);
  dialog.showModal();
  close.focus();
}

function buildCard(spec, { title, index, regen, fallback } = {}) {
  const stage = h("div", { class: "diagram-card__stage", role: "img", "aria-label": `${spec.label} of ${title || "this page"}` });
  const expand = h("button", { type: "button", class: "diagram-btn", "aria-label": `Expand ${spec.label.toLowerCase()}` }, "Expand");
  const card = { spec, title: title || "brain", index, regen, fallback, stage, svgText: null, colours: null };
  const menu = exportMenu(card);
  const figure = h(
    "figure",
    { class: "diagram-card", "data-type": spec.type },
    h(
      "figcaption",
      { class: "diagram-card__head" },
      h("span", { class: "diagram-card__kind" }, spec.label),
      h("span", { class: "diagram-card__note" }, "generated from the graph"),
      h("span", { class: "diagram-card__tools" }, menu, expand),
    ),
    stage,
    listLinks({ spec }),
  );
  card.figure = figure;
  expand.addEventListener("click", () => openFullscreen(card, expand));
  cards.add(card);
  return card;
}

/** Redraws every live card when the theme or the system preference changes. */
function listenForTheme() {
  if (listening) return;
  listening = true;
  const redraw = () => {
    for (const card of [...cards]) {
      if (!card.figure.isConnected) {
        dropSheet(card);
        cards.delete(card);
        continue;
      }
      if (card.svgText) draw(card);
    }
  };
  document.addEventListener("lb-theme", redraw);
  window.matchMedia("(prefers-color-scheme: light)").addEventListener?.("change", redraw);
}

/**
 * Replaces each ```mermaid figure in `root` with a card. The Mermaid build is
 * loaded only when there is one to draw. A source that fails keeps its code.
 */
export async function mountMermaidBlocks(root, { title } = {}) {
  const figures = [...root.querySelectorAll("figure.mermaid-block:not([data-mounted])")];
  if (!figures.length) return;
  await loadMermaid().catch(() => null);
  if (!globalThis.mermaid) return;
  listenForTheme();
  for (const figure of figures) {
    const source = figure.querySelector("code")?.textContent || "";
    if (!source.trim()) continue;
    const spec = { type: "mermaid", label: "Diagram", source, links: {}, items: [] };
    const card = buildCard(spec, { title, fallback: figure });
    card.figure.dataset.mounted = "true";
    figure.replaceWith(card.figure);
    await draw(card);
  }
}

/* ------------------------------------------------------------ page */

/** `GET /v1/graph`, once a minute at most: the graph is the same for every page. */
function graphFor(api) {
  const now = Date.now();
  if (graphMemo && now - graphMemo.at < GRAPH_TTL) return graphMemo.promise;
  const promise = Promise.resolve()
    .then(() => api("/v1/graph?limit=2000"))
    .catch(() => null);
  graphMemo = { at: now, promise };
  return promise;
}

/** The page's own node in the graph: its slug, preferring its own scope. */
function focusNode(graph, slug, scope) {
  const matches = (graph?.nodes || []).filter((node) => node.slug === slug);
  const own = matches.find((node) => scope && String(node.id).startsWith(`page:${scope}:`));
  return (own || matches[0] || {}).id || null;
}

/**
 * Generates the page's diagrams (at most two) and shows them under the body.
 * `isCurrent` says whether the page is still the one on screen, so a slow
 * graph read cannot paint a diagram onto the next page.
 */
export async function mountPageDiagrams(container, { api, slug, scope, title, entityType, markdown, facts, isCurrent }) {
  container.replaceChildren();
  container.hidden = true;
  const graph = ENTITY_TYPES.has(entityType) ? await graphFor(api) : null;
  const focusId = graph ? focusNode(graph, slug, scope) : null;
  const generate = (colours) =>
    diagramsForPage({ entityType, title, markdown, graph, focusId, facts: flattenFacts(facts), colours });
  const specs = generate(currentPalette());
  if (!specs.length || !isCurrent()) return;
  await loadMermaid().catch(() => null);
  if (!globalThis.mermaid || !isCurrent()) return;
  listenForTheme();
  const heading = h("h2", { class: "diagrams__title" }, "Diagrams");
  container.append(heading);
  for (const [index, spec] of specs.entries()) {
    const card = buildCard(spec, {
      title,
      index,
      regen: () => generate(currentPalette())[index],
    });
    container.append(card.figure);
    await draw(card);
  }
  container.hidden = false;
}
