// The app shell: the sidebar, the phone drawer, the workspace switcher, the
// user menu and the ⌘K command palette — the same chrome on every page
// (docs/ux-navigation.md). The destinations come from `nav.js`; this file is
// only the DOM around them.
//
// Every node is built with `createElement`/`createElementNS` and text nodes,
// never parsed markup, like the rest of `app/`. Each part looks for its own
// elements and quietly stands down when a page lacks them, so the device
// pages and the tests can load `app.js` without any of it.

import {
  NAV,
  ICONS,
  activeId,
  askHref,
  matchDestinations,
  paletteOptions,
  paletteSearchPath,
} from "./nav.js";
import { sessionView } from "./session.js";
import { workspacesRequest, switchRequest } from "./members.js";

const SVG_NS = "http://www.w3.org/2000/svg";
const COLLAPSE_KEY = "lb-nav-collapsed";
const SEARCH_DEBOUNCE_MS = 160;

/** Reads a per-browser preference; storage can be refused, so never throws. */
function readPref(key) {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function writePref(key, value) {
  try {
    if (value == null) localStorage.removeItem(key);
    else localStorage.setItem(key, value);
  } catch {
    /* a refused write costs the memory, not the click */
  }
}

/** A line icon from `ICONS`, or null where SVG cannot be built. */
export function icon(doc, name) {
  const paths = ICONS[name];
  if (!paths || typeof doc.createElementNS !== "function") return null;
  const svg = doc.createElementNS(SVG_NS, "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("width", "18");
  svg.setAttribute("height", "18");
  svg.setAttribute("aria-hidden", "true");
  svg.setAttribute("focusable", "false");
  svg.setAttribute("class", "ico");
  for (const d of paths) {
    const path = doc.createElementNS(SVG_NS, "path");
    path.setAttribute("d", d);
    svg.appendChild(path);
  }
  return svg;
}

function textEl(doc, tag, text, className) {
  const node = doc.createElement(tag);
  if (className) node.className = className;
  node.textContent = text;
  return node;
}

/**
 * Builds the sidebar's groups into `nav`: a label and a list per group, a
 * link per destination, and a non-link "Soon" row per reserved slot.
 * Returns the links by destination id, for the current-item marking.
 */
export function renderSidebar(doc, nav, groups = NAV) {
  nav.replaceChildren();
  const links = new Map();
  for (const group of groups) {
    const section = doc.createElement("div");
    section.className = "sidebar__group";
    const label = textEl(doc, "p", group.label, "sidebar__label");
    label.setAttribute("id", `nav-group-${group.id}`);
    section.appendChild(label);
    const list = doc.createElement("ul");
    list.className = "sidebar__list";
    list.setAttribute("aria-labelledby", `nav-group-${group.id}`);
    for (const item of group.items) {
      const li = doc.createElement("li");
      const row = doc.createElement(item.href ? "a" : "span");
      row.className = item.href ? "sidebar__link" : "sidebar__link sidebar__link--soon";
      if (item.href) row.setAttribute("href", item.href);
      else {
        row.setAttribute("aria-disabled", "true");
        row.setAttribute("title", item.soon || "Coming soon");
      }
      row.setAttribute("data-nav", item.id);
      const glyph = icon(doc, item.icon);
      if (glyph) row.appendChild(glyph);
      row.appendChild(textEl(doc, "span", item.label, "sidebar__text"));
      if (!item.href) row.appendChild(textEl(doc, "span", "Soon", "pill sidebar__soon"));
      li.appendChild(row);
      list.appendChild(li);
      links.set(item.id, row);
    }
    section.appendChild(list);
    nav.appendChild(section);
  }
  return links;
}

/** Marks one destination as the current page and clears the rest. */
export function markActive(links, id) {
  for (const [key, link] of links) {
    if (key === id) link.setAttribute("aria-current", "page");
    else link.removeAttribute("aria-current");
  }
}

/** Whether a keystroke is aimed at a field that wants its own keys. */
function typingIn(target) {
  const tag = target && typeof target.tagName === "string" ? target.tagName.toLowerCase() : "";
  return tag === "input" || tag === "textarea" || tag === "select" || Boolean(target && target.isContentEditable);
}

/**
 * A disclosure menu: the button toggles it, Escape and a click outside close
 * it, the arrow keys move between its items. `onOpen` may refill it first.
 */
function wireMenu(doc, button, menu, onOpen) {
  if (!button || !menu) return { close() {} };
  const items = () => Array.from(menu.querySelectorAll("a, button")).filter((item) => !item.disabled);
  const close = (focusButton = false) => {
    if (menu.hidden) return;
    menu.hidden = true;
    button.setAttribute("aria-expanded", "false");
    if (focusButton && typeof button.focus === "function") button.focus();
  };
  const open = async () => {
    menu.hidden = false;
    button.setAttribute("aria-expanded", "true");
    if (onOpen) await onOpen();
    const first = items()[0];
    if (first && typeof first.focus === "function") first.focus();
  };
  button.addEventListener("click", () => (menu.hidden ? open() : close()));
  menu.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      event.preventDefault();
      close(true);
      return;
    }
    if (event.key !== "ArrowDown" && event.key !== "ArrowUp") return;
    event.preventDefault();
    const list = items();
    if (!list.length) return;
    const at = list.indexOf(doc.activeElement);
    const next = event.key === "ArrowDown" ? (at + 1) % list.length : (at - 1 + list.length) % list.length;
    list[next].focus();
  });
  doc.addEventListener("click", (event) => {
    const target = event && event.target;
    if (menu.hidden || !target || typeof target.closest !== "function") return;
    if (!menu.contains(target) && !button.contains(target)) close();
  });
  return { close };
}

/**
 * Mounts the shell on this page. `deps` carries what lives in `app.js`:
 * `api`, `toggleTheme` and `signOut`. Returns `{ applySession, setActive }`
 * so the page can tell the shell who is signed in and which view it shows.
 */
export function mountShell(deps) {
  const doc = document;
  const { api } = deps;
  const body = doc.body;
  const nav = doc.getElementById("app-nav");
  const links = nav ? renderSidebar(doc, nav) : new Map();
  let current = activeId(location);
  markActive(links, current);

  /* -- collapse (desktop) and the drawer (phones) ------------------- */

  const collapse = doc.getElementById("nav-collapse");
  const paintCollapse = (collapsed) => {
    if (collapsed) body.dataset.navCollapsed = "true";
    else delete body.dataset.navCollapsed;
    if (!collapse) return;
    collapse.setAttribute("aria-pressed", String(collapsed));
    collapse.setAttribute("aria-label", collapsed ? "Expand the sidebar" : "Collapse the sidebar");
    collapse.replaceChildren();
    const glyph = icon(doc, collapsed ? "expand" : "collapse");
    if (glyph) collapse.appendChild(glyph);
    collapse.appendChild(textEl(doc, "span", collapsed ? "Expand" : "Collapse", "sidebar__text"));
  };
  paintCollapse(readPref(COLLAPSE_KEY) === "1");
  if (collapse) {
    collapse.addEventListener("click", () => {
      const next = body.dataset.navCollapsed !== "true";
      writePref(COLLAPSE_KEY, next ? "1" : null);
      paintCollapse(next);
    });
  }

  const toggle = doc.getElementById("nav-toggle");
  const scrim = doc.getElementById("nav-scrim");
  const sidebar = doc.getElementById("app-sidebar");
  const drawerOpen = () => body.dataset.nav === "open";
  const closeDrawer = (focusToggle = false) => {
    if (!drawerOpen()) return;
    delete body.dataset.nav;
    if (toggle) toggle.setAttribute("aria-expanded", "false");
    if (scrim) scrim.hidden = true;
    if (focusToggle && toggle && typeof toggle.focus === "function") toggle.focus();
  };
  const openDrawer = () => {
    body.dataset.nav = "open";
    if (toggle) toggle.setAttribute("aria-expanded", "true");
    if (scrim) scrim.hidden = false;
    const first = nav && nav.querySelector("a");
    if (first && typeof first.focus === "function") first.focus();
  };
  if (toggle) toggle.addEventListener("click", () => (drawerOpen() ? closeDrawer(true) : openDrawer()));
  if (scrim) scrim.addEventListener("click", () => closeDrawer(true));
  if (sidebar) {
    sidebar.addEventListener("click", (event) => {
      const target = event && event.target;
      if (target && typeof target.closest === "function" && target.closest("a")) closeDrawer();
    });
  }

  /* -- the workspace switcher and the user menu ---------------------- */

  const wsBox = doc.getElementById("ws-switch");
  const wsButton = doc.getElementById("ws-button");
  const wsMenu = doc.getElementById("ws-menu");
  const send = (request) =>
    api(request.path, request.body === undefined ? { method: request.method } : { method: request.method, body: request.body });

  const fillWorkspaces = async () => {
    if (!wsMenu) return;
    wsMenu.replaceChildren(textEl(doc, "p", "Loading…", "menu__note"));
    let list = [];
    try {
      const answer = await send(workspacesRequest());
      list = Array.isArray(answer) ? answer : [];
    } catch {
      list = [];
    }
    wsMenu.replaceChildren();
    wsMenu.appendChild(textEl(doc, "p", "Your workspaces", "menu__label"));
    for (const space of list) {
      const item = doc.createElement("button");
      item.type = "button";
      item.className = "menu__item";
      item.textContent = String(space.name || space.id || "");
      if (space.current) {
        item.disabled = true;
        item.setAttribute("aria-current", "true");
        item.appendChild(textEl(doc, "span", " · current", "menu__hint"));
      }
      else {
        item.addEventListener("click", async () => {
          item.disabled = true;
          try {
            await send(switchRequest(space.id));
            location.assign("/");
          } catch {
            item.disabled = false;
            item.textContent = `${String(space.name || space.id || "")} — could not switch`;
          }
        });
      }
      wsMenu.appendChild(item);
    }
    if (!list.length) wsMenu.appendChild(textEl(doc, "p", "Could not list your workspaces.", "menu__note"));
    const manage = doc.createElement("a");
    manage.className = "menu__item";
    manage.setAttribute("href", "settings.html#members");
    manage.textContent = "Members and invitations";
    wsMenu.appendChild(manage);
  };
  const wsControl = wireMenu(doc, wsButton, wsMenu, fillWorkspaces);
  const userControl = wireMenu(doc, doc.getElementById("user-button"), doc.getElementById("user-menu"));
  const userTheme = doc.getElementById("user-theme");
  if (userTheme) {
    userTheme.addEventListener("click", () => {
      userControl.close(true);
      deps.toggleTheme();
    });
  }

  /** Shows who is signed in where: the switcher, the avatar initial, the
   * `[data-session]` slots outside the sign-in page, and `data-session` on
   * `<body>`, which the sheet reads (the sign-in page hides the sidebar
   * signed out). */
  function applySession(state) {
    const signedIn = Boolean(state && state.kind === "signed-in");
    body.dataset.session = signedIn ? "signed-in" : state && state.kind === "checking" ? "checking" : "signed-out";
    if (!signedIn) {
      if (wsBox) wsBox.hidden = true;
      return;
    }
    const view = sessionView(state.me, deps.origin || "");
    if (wsBox) wsBox.hidden = false;
    if (body.dataset.page === "signin") doc.title = `${view.workspaceName} — Living Brain`;
    const name = doc.getElementById("ws-name");
    if (name) name.textContent = view.workspaceName;
    const initial = doc.getElementById("user-initial");
    if (initial) initial.textContent = (view.who || "?").trim().charAt(0).toUpperCase() || "?";
    if (body.dataset.page !== "signin") {
      for (const slot of doc.querySelectorAll("[data-session]")) {
        const key = slot.getAttribute("data-session");
        if (key && Object.prototype.hasOwnProperty.call(view, key)) slot.textContent = view[key];
      }
    }
  }

  /* -- the command palette -------------------------------------------- */

  const palette = buildPalette(doc);
  const input = palette.input;
  const list = palette.list;
  let options = [];
  let active = 0;
  let pages = [];
  let searchTimer = null;
  let searchSeq = 0;
  const signedIn = () => body.dataset.session === "signed-in";

  const paint = () => {
    list.replaceChildren();
    let lastGroup = "";
    options.forEach((option, at) => {
      if (option.group !== lastGroup) {
        lastGroup = option.group;
        const head = textEl(doc, "li", option.group, "palette__group");
        head.setAttribute("role", "presentation");
        list.appendChild(head);
      }
      const row = doc.createElement("li");
      row.className = at === active ? "palette__opt palette__opt--active" : "palette__opt";
      row.setAttribute("role", "option");
      row.setAttribute("id", `palette-opt-${at}`);
      row.setAttribute("aria-selected", String(at === active));
      row.appendChild(textEl(doc, "span", option.label, "palette__label"));
      if (option.detail) row.appendChild(textEl(doc, "span", option.detail, "palette__detail"));
      row.addEventListener("click", () => choose(at));
      row.addEventListener("mousemove", () => {
        if (active !== at) {
          active = at;
          paintActive();
        }
      });
      list.appendChild(row);
    });
    if (!options.length) list.appendChild(textEl(doc, "li", "Nothing matches.", "palette__none"));
    paintActive();
  };

  const paintActive = () => {
    for (const row of list.querySelectorAll("[role=option]")) {
      const at = Number(String(row.getAttribute("id")).replace("palette-opt-", ""));
      row.className = at === active ? "palette__opt palette__opt--active" : "palette__opt";
      row.setAttribute("aria-selected", String(at === active));
      if (at === active && typeof row.scrollIntoView === "function") row.scrollIntoView({ block: "nearest" });
    }
    if (options[active]) input.setAttribute("aria-activedescendant", `palette-opt-${active}`);
    else input.removeAttribute("aria-activedescendant");
  };

  const refresh = () => {
    options = paletteOptions(input.value, { pages, signedIn: signedIn() });
    active = 0;
    paint();
  };

  const runSearch = async (query) => {
    const seq = ++searchSeq;
    try {
      const data = await api(paletteSearchPath(query));
      if (seq !== searchSeq) return;
      pages = data && Array.isArray(data.results) ? data.results : [];
    } catch {
      if (seq !== searchSeq) return;
      pages = [];
    }
    refresh();
  };

  const onInput = () => {
    clearTimeout(searchTimer);
    pages = [];
    refresh();
    const query = input.value.trim();
    if (query.length >= 2 && signedIn()) {
      searchTimer = setTimeout(() => runSearch(query), SEARCH_DEBOUNCE_MS);
    }
  };

  const choose = (at) => {
    const option = options[at];
    if (!option) return;
    closePalette();
    if (option.action === "theme") deps.toggleTheme();
    else if (option.action === "signout") deps.signOut();
    else if (option.href) location.assign(option.href);
  };

  let returnFocus = null;
  function openPalette() {
    if (palette.dialog.open) return;
    returnFocus = doc.activeElement;
    input.value = "";
    pages = [];
    refresh();
    if (typeof palette.dialog.showModal === "function") palette.dialog.showModal();
    else palette.dialog.setAttribute("open", "");
    input.setAttribute("aria-expanded", "true");
    if (typeof input.focus === "function") input.focus();
  }
  function closePalette() {
    clearTimeout(searchTimer);
    searchSeq += 1;
    if (typeof palette.dialog.close === "function" && palette.dialog.open) palette.dialog.close();
    else palette.dialog.removeAttribute("open");
    input.setAttribute("aria-expanded", "false");
    if (returnFocus && typeof returnFocus.focus === "function") returnFocus.focus();
    returnFocus = null;
  }

  input.addEventListener("input", onInput);
  input.addEventListener("keydown", (event) => {
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      if (!options.length) return;
      active = event.key === "ArrowDown" ? (active + 1) % options.length : (active - 1 + options.length) % options.length;
      paintActive();
    } else if (event.key === "Enter") {
      event.preventDefault();
      choose(active);
    } else if (event.key === "Escape") {
      event.preventDefault();
      closePalette();
    }
  });
  palette.dialog.addEventListener("click", (event) => {
    // A click on the backdrop (the dialog box itself, outside the panel).
    if (event && event.target === palette.dialog) closePalette();
  });
  palette.dialog.addEventListener("cancel", (event) => {
    if (event && typeof event.preventDefault === "function") event.preventDefault();
    closePalette();
  });
  const opener = doc.getElementById("palette-open");
  if (opener) opener.addEventListener("click", openPalette);
  // The shortcut as this keyboard spells it.
  const mac = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(String(navigator.platform || navigator.userAgent || ""));
  const kbd = doc.querySelector(".topbar__kbd");
  if (kbd) kbd.textContent = mac ? "⌘K" : "Ctrl K";

  doc.addEventListener("keydown", (event) => {
    if (!event || typeof event.key !== "string") return;
    const key = event.key.toLowerCase();
    if (key === "k" && (event.metaKey || event.ctrlKey) && !event.altKey) {
      event.preventDefault();
      if (palette.dialog.open) closePalette();
      else openPalette();
      return;
    }
    if (event.key === "/" && !event.metaKey && !event.ctrlKey && !event.altKey) {
      // On the Ask view "/" belongs to its own box (`wiki.js`).
      if (typingIn(event.target) || body.dataset.view === "ask" || palette.dialog.open) return;
      if (body.dataset.page === "signin" && !signedIn()) return;
      event.preventDefault();
      openPalette();
      return;
    }
    if (event.key === "Escape") {
      if (drawerOpen()) closeDrawer(true);
      wsControl.close();
      userControl.close();
    }
  });

  return {
    applySession,
    /** The page says which view it shows (`wiki.js` switches views in place). */
    setActive(view) {
      current = activeId(location, view);
      markActive(links, current);
    },
    openPalette,
    closePalette,
    get active() {
      return current;
    },
    askHref,
    destinations: matchDestinations,
  };
}

/** The palette's dialog, built once and appended to `<body>`. */
function buildPalette(doc) {
  const dialog = doc.createElement("dialog");
  dialog.className = "dialog palette";
  dialog.setAttribute("id", "palette");
  dialog.setAttribute("aria-label", "Search or ask");
  const panel = doc.createElement("div");
  panel.className = "palette__panel";
  const label = textEl(doc, "label", "Search or ask", "sr");
  label.setAttribute("for", "palette-input");
  const input = doc.createElement("input");
  input.setAttribute("id", "palette-input");
  input.setAttribute("type", "search");
  input.setAttribute("autocomplete", "off");
  input.setAttribute("spellcheck", "false");
  input.setAttribute("placeholder", "Jump to…, find a page, or ask a question");
  input.setAttribute("role", "combobox");
  input.setAttribute("aria-expanded", "false");
  input.setAttribute("aria-controls", "palette-list");
  input.setAttribute("aria-autocomplete", "list");
  input.className = "palette__input";
  const list = doc.createElement("ul");
  list.setAttribute("id", "palette-list");
  list.setAttribute("role", "listbox");
  list.setAttribute("aria-label", "Results");
  list.className = "palette__list";
  const foot = textEl(doc, "p", "↑ ↓ to move · Enter to open · Esc to close", "palette__foot tiny");
  panel.append(label, input, list, foot);
  dialog.appendChild(panel);
  if (doc.body && typeof doc.body.appendChild === "function") doc.body.appendChild(dialog);
  return { dialog, input, list };
}
