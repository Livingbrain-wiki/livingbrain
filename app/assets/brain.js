// The Brain map page (`brain.html`): the asker's real brain — pages,
// sources, projects and fact subjects from `GET /v1/graph` — as a 3D graph
// in the website brain's style, with filters, search-to-focus, a time slider
// that replays the brain growing, a side panel for the picked node, and a
// plain list for anyone the canvas does not serve.
//
// `app.js` imports `brainPage` and its boot dispatches it for
// `body[data-page="brain"]`. Every node reaching the document is built with
// `createElement` and text; the page body in the side panel goes through the
// Markdown renderer like every other page body.

import {
  KINDS,
  graphPath,
  normalizeGraph,
  visibleMask,
  searchNodes,
  nodeHref,
  tooltipLines,
  kindName,
  createLayout,
} from "./graph.js";
import { mountBrain } from "./brain-render.js";
import { renderMarkdown } from "./markdown.js";
import { linkWikilinks } from "./wiki.js";

/** The node count the page asks for: the API's ceiling. */
export const MAP_LIMIT = 2000;
/** How many rows the accessible list shows. */
const LIST_MAX = 250;

/** A page body for the side panel: no frontmatter fence, and no leading
 * `# Title` — the panel's own heading already says it. */
export function panelMarkdown(markdown) {
  let source = String(markdown == null ? "" : markdown);
  const lines = source.split("\n");
  if (lines[0] === "---") {
    const close = lines.indexOf("---", 1);
    if (close > 0) source = lines.slice(close + 1).join("\n");
  }
  return source.replace(/^\s*# [^\n]*\n+/, "");
}

function text(tag, value, className) {
  const el = document.createElement(tag);
  if (className) el.className = className;
  el.textContent = value;
  return el;
}

function day(ms) {
  return new Date(ms).toISOString().slice(0, 10);
}

/**
 * The page. `deps` is `{ api, ApiError, notice, renderInto, checkSession }`
 * from `app.js`, passed in so this module never imports the page glue back.
 */
export async function brainPage(deps) {
  const { api, ApiError, notice, renderInto, checkSession } = deps;
  const $ = (id) => document.getElementById(id);
  const els = {
    notice: $("map-notice"),
    status: $("map-status"),
    stage: $("map-stage"),
    canvas: $("map-canvas"),
    tip: $("map-tip"),
    empty: $("map-empty"),
    search: $("map-search"),
    results: $("map-results"),
    project: $("map-project"),
    agent: $("map-agent"),
    legend: $("map-legend"),
    spin: $("map-spin"),
    flat: $("map-flat"),
    reset: $("map-reset"),
    zoomIn: $("map-zoom-in"),
    zoomOut: $("map-zoom-out"),
    time: $("map-time"),
    timeLabel: $("map-time-label"),
    play: $("map-play"),
    panel: $("map-panel"),
    panelClose: $("map-panel-close"),
    panelKind: $("map-panel-kind"),
    panelTitle: $("map-panel-title"),
    panelMeta: $("map-panel-meta"),
    panelBody: $("map-panel-body"),
    panelOpen: $("map-panel-open"),
    list: $("map-list"),
    listBody: $("map-list-body"),
  };
  const reduced =
    typeof window.matchMedia === "function" && window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  const session = await checkSession(api);
  if (session.kind !== "signed-in") {
    els.stage.hidden = true;
    notice(els.notice, "Sign in to see your brain map.", "warn");
    const link = text("a", "Sign in");
    link.setAttribute("href", "index.html");
    els.notice.appendChild(document.createTextNode(" "));
    els.notice.appendChild(link);
    return;
  }

  const filters = { kinds: null, project: "", agent: "", until: null };
  let graph = null;
  let view = null;
  let playing = 0;
  let selected = null;
  let fetchedFor = "";

  els.status.textContent = "Reading your brain…";
  async function load(project = "") {
    const answer = await api(graphPath({ project, limit: MAP_LIMIT }));
    fetchedFor = project;
    return normalizeGraph(answer);
  }
  try {
    graph = await load();
  } catch (error) {
    els.status.textContent = "";
    if (error instanceof ApiError && error.status === 401) {
      notice(els.notice, "Sign in to see your brain map.", "warn");
    } else {
      notice(els.notice, `Could not read the map: ${error.message}`, "bad");
    }
    els.stage.hidden = true;
    return;
  }

  /* -- the facets ------------------------------------------------------ */

  function fillSelect(select, all, entries) {
    select.replaceChildren();
    const first = text("option", all);
    first.value = "";
    select.appendChild(first);
    for (const entry of entries) {
      const option = text("option", entry.label);
      option.value = entry.value;
      select.appendChild(option);
    }
    select.hidden = entries.length === 0;
  }

  const counts = () => {
    const byKind = new Map();
    for (const node of graph.nodes) byKind.set(node.kind, (byKind.get(node.kind) || 0) + 1);
    return byKind;
  };

  function paintLegend() {
    els.legend.replaceChildren();
    const byKind = counts();
    for (const kind of graph.kinds) {
      const entry = KINDS.find((item) => item.kind === kind);
      const button = document.createElement("button");
      button.type = "button";
      button.className = "map-chip";
      button.dataset.kind = kind;
      button.setAttribute("aria-pressed", String(!filters.kinds || filters.kinds.has(kind)));
      const dot = text("span", "", "map-chip__dot");
      dot.setAttribute("aria-hidden", "true");
      dot.style.setProperty("--c", `var(${entry.token})`);
      button.append(dot, text("span", entry.label), text("span", String(byKind.get(kind) || 0), "map-chip__n"));
      button.addEventListener("click", () => {
        const all = new Set(graph.kinds);
        const on = filters.kinds ? new Set(filters.kinds) : all;
        if (on.has(kind)) on.delete(kind);
        else on.add(kind);
        filters.kinds = on.size === all.size ? null : on;
        paintLegend();
        apply();
      });
      els.legend.appendChild(button);
    }
  }

  function paintFacets() {
    fillSelect(
      els.project,
      "All projects",
      graph.projects.map((entry) => ({ value: entry.id, label: entry.name })),
    );
    els.project.value = filters.project;
    fillSelect(els.agent, "All agents", graph.agents.map((agent) => ({ value: agent, label: agent })));
    els.agent.value = filters.agent;
    paintLegend();
    const span = graph.start != null && graph.end != null && graph.end > graph.start;
    els.time.disabled = !span;
    els.play.disabled = !span || reduced;
    els.play.hidden = reduced;
    els.time.min = "0";
    els.time.max = "1000";
    els.time.value = "1000";
    paintTime();
  }

  function paintTime() {
    if (graph.start == null || graph.end == null || graph.end <= graph.start) {
      els.timeLabel.textContent = graph.end != null ? `As of ${day(graph.end)}` : "No dates yet";
      filters.until = null;
      return;
    }
    const k = Number(els.time.value) / 1000;
    const at = graph.start + (graph.end - graph.start) * k;
    filters.until = k >= 1 ? null : at;
    els.timeLabel.textContent = k >= 1 ? `Now · since ${day(graph.start)}` : `As of ${day(at)}`;
    els.time.setAttribute("aria-valuetext", els.timeLabel.textContent);
  }

  /* -- status, list, apply ------------------------------------------- */

  function paintStatus(mask) {
    let shown = 0;
    for (const on of mask) shown += on;
    const links = graph.edges.filter((edge) => mask[edge.a] && mask[edge.b]).length;
    const parts = [`${shown.toLocaleString()} of ${graph.nodes.length.toLocaleString()} nodes`, `${links.toLocaleString()} links`];
    if (graph.truncated && graph.counts) {
      parts.push(`the ${graph.nodes.length.toLocaleString()} most connected of ${Number(graph.counts.nodes_total || 0).toLocaleString()}`);
    }
    els.status.textContent = parts.join(" · ");
  }

  function paintList(mask) {
    els.listBody.replaceChildren();
    const rows = graph.nodes.filter((node) => mask[node.i]).sort((a, b) => b.weight - a.weight).slice(0, LIST_MAX);
    for (const node of rows) {
      const li = document.createElement("li");
      const href = nodeHref(node);
      const name = href ? text("a", node.label) : text("span", node.label);
      if (href) name.setAttribute("href", href);
      li.append(name, text("span", ` · ${tooltipLines(node).slice(1).join(" · ")}`, "muted"));
      els.listBody.appendChild(li);
    }
  }

  function apply() {
    const mask = visibleMask(graph, filters);
    if (view) view.setMask(mask);
    paintStatus(mask);
    if (els.list.open) paintList(mask);
    els.empty.hidden = graph.nodes.length !== 0;
  }

  /* -- the side panel ------------------------------------------------ */

  async function openPanel(node) {
    selected = node;
    if (view) view.select(node.i, true);
    els.panel.hidden = false;
    els.panelKind.textContent = kindName(node);
    els.panelTitle.textContent = node.label;
    els.panelMeta.textContent = tooltipLines(node).slice(1).join(" · ");
    const href = nodeHref(node);
    els.panelOpen.hidden = !href;
    if (href) els.panelOpen.setAttribute("href", href);
    els.panelOpen.textContent = node.slug ? "Open the page" : node.kind === "session" ? "Open the transcript" : "Open";
    els.panelBody.replaceChildren();
    if (node.slug) {
      els.panelBody.appendChild(text("p", "Loading…", "tiny"));
      try {
        const page = await api(`/v1/pages/${encodeURIComponent(node.slug)}`);
        if (selected !== node) return;
        if (page && page.title) els.panelTitle.textContent = page.title;
        renderInto(els.panelBody, renderMarkdown(panelMarkdown(page && page.markdown)));
        linkWikilinks(els.panelBody);
      } catch (error) {
        if (selected !== node) return;
        els.panelBody.replaceChildren(text("p", `Could not open this page: ${error.message}`, "tiny"));
      }
    } else {
      const neighbours = graph.edges
        .filter((edge) => edge.a === node.i || edge.b === node.i)
        .map((edge) => graph.nodes[edge.a === node.i ? edge.b : edge.a])
        .sort((a, b) => b.weight - a.weight)
        .slice(0, 12);
      if (neighbours.length) {
        els.panelBody.appendChild(text("h3", "Connected to"));
        const list = document.createElement("ul");
        list.className = "map-panel__links";
        for (const other of neighbours) {
          const li = document.createElement("li");
          const button = text("button", other.label, "linklike");
          button.type = "button";
          button.addEventListener("click", () => openPanel(other));
          li.appendChild(button);
          list.appendChild(li);
        }
        els.panelBody.appendChild(list);
      }
    }
    if (typeof els.panelClose.focus === "function") els.panelClose.focus();
  }

  function closePanel() {
    selected = null;
    els.panel.hidden = true;
    if (view) view.select(-1);
    if (typeof els.canvas.focus === "function") els.canvas.focus();
  }

  /* -- the tooltip ---------------------------------------------------- */

  function hover(node, x, y) {
    if (!node) {
      els.tip.hidden = true;
      return;
    }
    els.tip.replaceChildren();
    const lines = tooltipLines(node);
    els.tip.appendChild(text("strong", lines[0]));
    for (const line of lines.slice(1)) els.tip.appendChild(text("span", line));
    els.tip.hidden = false;
    const box = els.stage.getBoundingClientRect();
    const left = Math.min(Math.max(8, x + 14), Math.max(8, box.width - 240));
    const top = Math.min(Math.max(8, y + 14), Math.max(8, box.height - 110));
    els.tip.style.transform = `translate(${Math.round(left)}px, ${Math.round(top)}px)`;
  }

  /* -- mount ---------------------------------------------------------- */

  paintFacets();
  if (!graph.nodes.length) {
    els.status.textContent = "";
    els.empty.hidden = false;
    els.canvas.hidden = true;
    return;
  }
  const canvasOk = Boolean(els.canvas.getContext && els.canvas.getContext("2d"));
  if (canvasOk) {
    const layout = createLayout(graph);
    view = mountBrain(els.canvas, graph, layout, {
      reduced,
      tokens: KINDS.map((entry) => entry.token),
      onHover: hover,
      onPick: openPanel,
    });
  }
  if (!view) {
    // No canvas: the list is the map.
    els.canvas.hidden = true;
    els.list.open = true;
  }
  els.spin.setAttribute("aria-pressed", String(Boolean(view && view.spinning)));
  els.spin.hidden = reduced;
  apply();
  if (els.list.open) paintList(visibleMask(graph, filters));

  /* -- controls --------------------------------------------------------- */

  els.list.addEventListener("toggle", () => {
    if (els.list.open) paintList(visibleMask(graph, filters));
  });
  els.project.addEventListener("change", async () => {
    filters.project = els.project.value;
    // A truncated map may have left the project's nodes out: ask for the
    // project's own best nodes, filtered before the server's limit.
    if (graph.truncated && filters.project && fetchedFor !== filters.project) {
      try {
        const next = await load(filters.project);
        if (next.nodes.length) {
          if (view) view.destroy();
          graph = next;
          view = mountBrain(els.canvas, graph, createLayout(graph), {
            reduced,
            tokens: KINDS.map((entry) => entry.token),
            onHover: hover,
            onPick: openPanel,
          });
          paintFacets();
          els.project.value = filters.project;
        }
      } catch {
        /* the loaded map, filtered, is still a map */
      }
    }
    apply();
  });
  els.agent.addEventListener("change", () => {
    filters.agent = els.agent.value;
    apply();
  });
  els.time.addEventListener("input", () => {
    paintTime();
    apply();
  });
  els.play.addEventListener("click", () => {
    if (playing) {
      cancelAnimationFrame(playing);
      playing = 0;
      els.play.textContent = "Play";
      els.play.setAttribute("aria-pressed", "false");
      return;
    }
    if (Number(els.time.value) >= 1000) els.time.value = "0";
    els.play.textContent = "Pause";
    els.play.setAttribute("aria-pressed", "true");
    let last = 0;
    const frame = (now) => {
      const dt = last ? now - last : 16;
      last = now;
      els.time.value = String(Math.min(1000, Number(els.time.value) + dt * 0.08));
      paintTime();
      apply();
      if (Number(els.time.value) >= 1000) {
        playing = 0;
        els.play.textContent = "Play";
        els.play.setAttribute("aria-pressed", "false");
        return;
      }
      playing = requestAnimationFrame(frame);
    };
    playing = requestAnimationFrame(frame);
  });
  if (view) {
    els.spin.addEventListener("click", () => {
      view.setSpin(!view.spinning);
      els.spin.setAttribute("aria-pressed", String(view.spinning));
    });
    els.flat.addEventListener("click", () => {
      const on = els.flat.getAttribute("aria-pressed") !== "true";
      els.flat.setAttribute("aria-pressed", String(on));
      els.flat.textContent = on ? "3D" : "Flat";
      view.setFlat(on);
    });
    els.reset.addEventListener("click", () => {
      closePanel();
      view.reset();
    });
    els.zoomIn.addEventListener("click", () => view.zoomBy(1.25));
    els.zoomOut.addEventListener("click", () => view.zoomBy(0.8));
    els.canvas.addEventListener("keydown", (event) => {
      const keys = {
        ArrowLeft: () => view.rotateBy(-0.12),
        ArrowRight: () => view.rotateBy(0.12),
        ArrowUp: () => view.rotateBy(0, -0.08),
        ArrowDown: () => view.rotateBy(0, 0.08),
        "+": () => view.zoomBy(1.2),
        "=": () => view.zoomBy(1.2),
        "-": () => view.zoomBy(0.83),
        0: () => view.reset(),
      };
      const run = keys[event.key];
      if (run) {
        event.preventDefault();
        run();
      }
    });
  }
  els.panelClose.addEventListener("click", closePanel);
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && !els.panel.hidden) closePanel();
  });

  /* -- search to focus ---------------------------------------------- */

  let matches = [];
  let active = -1;
  function paintResults() {
    els.results.replaceChildren();
    matches.forEach((node, at) => {
      const option = text("li", "", at === active ? "map-results__opt is-active" : "map-results__opt");
      option.setAttribute("role", "option");
      option.setAttribute("id", `map-opt-${at}`);
      option.setAttribute("aria-selected", String(at === active));
      option.append(text("span", node.label), text("span", (KINDS[node.k] || {}).label || node.kind, "muted"));
      option.addEventListener("click", () => chooseMatch(at));
      els.results.appendChild(option);
    });
    els.results.hidden = matches.length === 0;
    els.search.setAttribute("aria-expanded", String(matches.length > 0));
    if (active >= 0) els.search.setAttribute("aria-activedescendant", `map-opt-${active}`);
    else els.search.removeAttribute("aria-activedescendant");
  }
  function chooseMatch(at) {
    const node = matches[at];
    if (!node) return;
    matches = [];
    active = -1;
    paintResults();
    openPanel(node);
  }
  els.search.addEventListener("input", () => {
    matches = searchNodes(graph, els.search.value);
    active = -1;
    paintResults();
    if (view) view.setFocus(matches);
  });
  els.search.addEventListener("keydown", (event) => {
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      if (!matches.length) return;
      event.preventDefault();
      active = event.key === "ArrowDown" ? (active + 1) % matches.length : (active - 1 + matches.length) % matches.length;
      paintResults();
    } else if (event.key === "Enter") {
      event.preventDefault();
      chooseMatch(active >= 0 ? active : 0);
    } else if (event.key === "Escape") {
      matches = [];
      active = -1;
      paintResults();
      if (view) view.setFocus([]);
    }
  });
}
