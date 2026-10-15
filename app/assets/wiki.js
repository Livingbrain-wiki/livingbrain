// The wiki page: home (list + pins), a page, its editor, the new-page form,
// the ask panel and header search.
//
// `app.js` imports `wikiPage` from here and its boot dispatches it for
// `body[data-page="wiki"]`, so this file is the one module the wiki page runs
// — the same functions these tests drive. It never sends an Authorization
// header (see the header comment in `app.js`): the session is the cookie, and
// a `401` from the API is the signed-out state, shown as such, never an error
// dialog.
//
// Everything that reaches the document goes through `mountNodes` /
// `createElement` + `textContent` — no `innerHTML` anywhere, like the rest of
// `app/`.

import {
  $,
  $$,
  ApiError,
  mountNodes,
  notice,
  renderInto,
} from "./app.js";
import {
  renderBacklinks,
  renderCitations,
  renderMarkdown,
  isSafeHref,
} from "./markdown.js";
import { applyHeader, checkSession, signOutRequest } from "./session.js";

/** Where the per-browser list of pinned `scope/slug` keys lives. */
const PIN_KEY = "lb-wiki-pinned";

/** The server's slug rule (crates/livingbrain-api): 1..128 of [a-z0-9-]. */
export const SLUG_PATTERN = /^[a-z0-9-]{1,128}$/;

/** The message the new-page form shows when a slug fails that rule. */
export const SLUG_MESSAGE =
  "Use lowercase letters, digits and hyphens — 1 to 128 characters.";

/** Header search: keystrokes settle for this long before a request goes out. */
const SEARCH_DEBOUNCE_MS = 200;

/** Header search: the API caps `limit` at 25; ten fits the panel. */
const SEARCH_LIMIT = 10;

/** What a `TreeWalker` over text nodes asks for: `NodeFilter.SHOW_TEXT`,
 * spelled out for DOM stubs running under Node, which has no `NodeFilter`. */
const SHOW_TEXT = 4;

/* --------------------------------------------------------- pure helpers */

/** Free text to a slug: lowercase, `[a-z0-9-]`, at most 128 chars, no dashes
 * at either end. Answers "" when nothing survives — callers decide what an
 * empty slug means for them. */
export function slugify(value) {
  return String(value == null ? "" : value)
    .trim()
    .toLowerCase()
    .replace(/['’]/g, "")
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+/, "")
    .slice(0, 128)
    .replace(/-+$/, "");
}

/** True when `value` is exactly what the server accepts as a slug. */
export function validSlug(value) {
  return typeof value === "string" && SLUG_PATTERN.test(value);
}

/**
 * A search-result URL (`…/brain/{scope}/{slug}`) to its two halves, or null
 * when the path does not name both. Relative URLs are judged against the
 * site's own host, which is where the server says these links live.
 */
export function parseBrainUrl(url) {
  let parsed;
  try {
    parsed = new URL(String(url == null ? "" : url), "https://livingbrain.wiki");
  } catch {
    return null;
  }
  const parts = parsed.pathname.split("/").filter(Boolean);
  const at = parts.indexOf("brain");
  if (at === -1 || parts.length - at < 3) return null;
  const scope = parts[at + 1];
  const slug = parts[at + 2];
  if (!scope || !slug) return null;
  return { scope, slug };
}

/**
 * Splits `value` around every occurrence of any term: text-node descriptions
 * between `mark` descriptions, longest terms first, case-insensitive. Terms
 * shorter than two characters are ignored, so a one-letter query never
 * shreds a paragraph. The output mounts through `mountNodes` like any other
 * node description — no HTML string is ever built.
 */
export function highlightTerms(value, terms) {
  const source = String(value == null ? "" : value);
  const wanted = [
    ...new Set(
      (Array.isArray(terms) ? terms : [])
        .map((term) => String(term).toLowerCase())
        .filter((term) => term.length >= 2),
    ),
  ].sort((a, b) => b.length - a.length);
  if (!source || !wanted.length) return source ? [tx(source)] : [];
  const lower = source.toLowerCase();
  const parts = [];
  let cursor = 0;
  while (cursor < source.length) {
    let best = -1;
    let bestTerm = "";
    for (const term of wanted) {
      const at = lower.indexOf(term, cursor);
      // `<` keeps the longest term when two match at the same index.
      if (at !== -1 && (best === -1 || at < best)) {
        best = at;
        bestTerm = term;
      }
    }
    if (best === -1) break;
    if (best > cursor) parts.push(tx(source.slice(cursor, best)));
    parts.push(el("mark", [tx(source.slice(best, best + bestTerm.length))]));
    cursor = best + bestTerm.length;
  }
  if (cursor < source.length) parts.push(tx(source.slice(cursor)));
  return parts;
}

/**
 * A wikilink body (the text between `[[` and `]]`) to its link, or null when
 * it must stay literal text. A bare `[[tamar-routes]]` names the address
 * itself, so it must already be a slug — lowercased first, so
 * `[[Tamar-Routes]]` links the same page. An aliased `[[orbit window|Orbit
 * Window]]` is free text naming a concept, so its target is slugified and the
 * alias becomes the label.
 */
export function wikilinkToLink(body) {
  const bar = String(body == null ? "" : body).indexOf("|");
  const target = (bar === -1 ? body : body.slice(0, bar)).trim();
  const alias = bar === -1 ? "" : body.slice(bar + 1).trim();
  const slug = alias ? slugify(target) : target.toLowerCase();
  if (!validSlug(slug)) return null;
  return { slug, label: alias || slug };
}

/**
 * The body after a leading frontmatter fence: when the very first line is
 * exactly `---`, everything up to the next line that is exactly `---` is
 * frontmatter, and what comes back is everything after that line's newline,
 * with at most one leading newline trimmed so the body starts clean. Without
 * a fence at the very start, or with no closing fence, the input comes back
 * unchanged. Display-only — the server's search snippets strip the same way,
 * but the editor's textarea and what Save sends keep the raw markdown.
 */
export function stripFrontmatter(markdown) {
  const source = String(markdown == null ? "" : markdown);
  const lines = source.split("\n");
  if (lines[0] !== "---") return source;
  for (let at = 1; at < lines.length; at += 1) {
    if (lines[at] !== "---") continue;
    let body = lines.slice(at + 1).join("\n");
    if (body.startsWith("\n")) body = body.slice(1);
    return body;
  }
  return source;
}

/* --------------------------------------------- tiny description builders */

function tx(value) {
  return { type: "text", value: String(value) };
}

function el(tag, children, attrs) {
  const node = { type: "element", tag, children: children || [] };
  if (attrs && Object.keys(attrs).length) node.attrs = attrs;
  return node;
}

/** The query's highlightable words: split on whitespace, two chars or more. */
function terms(query) {
  return String(query)
    .trim()
    .split(/\s+/)
    .filter((word) => word.length >= 2);
}

/** `highlightTerms`, but only the first match stays marked. */
function firstHighlight(value, list) {
  let seen = false;
  return highlightTerms(value, list).map((part) => {
    if (part.tag !== "mark" || seen) {
      return part.tag === "mark"
        ? tx(part.children.map((child) => child.value).join(""))
        : part;
    }
    seen = true;
    return part;
  });
}

/* ----------------------------------------------------------- the pins */

/** The stored pin keys, or [] when storage is empty, corrupt or refused. */
function readPins() {
  try {
    const value = JSON.parse(localStorage.getItem(PIN_KEY) || "[]");
    return Array.isArray(value) ? value.filter((v) => typeof v === "string") : [];
  } catch {
    return [];
  }
}

function writePins(pins) {
  try {
    localStorage.setItem(PIN_KEY, JSON.stringify(pins));
  } catch {
    /* storage can be refused; pins then last only this page view */
  }
}

/** A page's pin key. Scope rides in on list rows; on a page body it can only
 * be recovered from the `url` the server sends back, so that is where it is
 * read — the two agree because the server builds both. */
function pinId(row) {
  const where = parseBrainUrl(row && row.url);
  const scope = (row && row.scope) || (where && where.scope) || "";
  return `${scope}/${(row && row.slug) || ""}`;
}

/** A list row's `updated_at` as a locale date, "" when unparseable. */
function formatDate(value) {
  const date = new Date(value);
  if (!value || Number.isNaN(date.getTime())) return "";
  try {
    return date.toLocaleDateString(undefined, {
      month: "short",
      day: "numeric",
      year: "numeric",
    });
  } catch {
    return String(value);
  }
}

/* --------------------------------------------------------- the controller */

/**
 * The whole wiki page, one call per load. `api` is the app's fetch wrapper.
 * Everything the page does — session, routing, the four views, search, ask —
 * happens inside this closure, so a second call starts fresh.
 */
export async function wikiPage(api) {
  const params = new URLSearchParams(location.search);
  const startSlug = params.get("slug") || "";
  const startInEdit = params.get("mode") === "edit";

  const els = {
    views: {
      home: $("#home-view"),
      page: $("#page-view"),
      editor: $("#editor-view"),
      new: $("#new-page-view"),
    },
    notice: $("#wiki-notice"),
    status: $("#wiki-status"),
    homeStatus: $("#home-status"),
    listFilter: $("#list-filter"),
    pinnedSection: $("#pinned-section"),
    pinnedList: $("#pinned-list"),
    recentSection: $("#recent-section"),
    recentList: $("#recent-list"),
    homeNomatch: $("#home-nomatch"),
    homeEmpty: $("#home-empty"),
    pageTitle: $("#page-title"),
    pageMeta: $("#page-meta"),
    pageBody: $("#page-body"),
    pageBacklinks: $("#page-backlinks"),
    pageCitations: $("#page-citations"),
    tocDesktop: $("#toc-desktop"),
    tocList: $("#toc-list"),
    tocDrawerHeading: $("#toc-drawer-heading"),
    tocDrawer: $("#toc-drawer"),
    edit: $("#edit"),
    pinToggle: $("#pin-toggle"),
    askOpen: $("#ask-open"),
    askOpenEdit: $("#ask-open-edit"),
    editTitle: $("#edit-title"),
    editWrite: $("#edit-write"),
    editPreview: $("#edit-preview"),
    source: $("#source"),
    preview: $("#preview"),
    save: $("#save"),
    cancelEdit: $("#cancel-edit"),
    npForm: $("#np-form"),
    npSlug: $("#np-slug"),
    npSlugError: $("#np-slug-error"),
    npTitle: $("#np-title"),
    npCreate: $("#np-create"),
    npCancel: $("#np-cancel"),
    searchForm: $("#wiki-search-form"),
    searchInput: $("#wiki-search"),
    searchResults: $("#wiki-search-results"),
    drawerToggle: $("#drawer-toggle"),
    drawer: $("#wiki-drawer"),
    drawerSignin: $("#drawer-signin"),
    drawerSignout: $("#drawer-signout"),
    askPanel: $("#ask-panel"),
    askQuestion: $("#ask-question"),
    askSubmit: $("#ask-submit"),
    askClose: $("#ask-close"),
    askStatus: $("#ask-status"),
    askAnswer: $("#ask-answer"),
    askCitations: $("#ask-citations"),
  };

  /** The page's state. One load of `wikiPage` owns one of these. */
  const state = {
    signedIn: false,
    session: null,
    slug: startSlug,
    page: null,
    version: null,
    rows: [],
    filter: "",
    searchResults: [],
    searchOptions: [],
    activeSearch: -1,
    previewing: false,
  };
  let searchTimer = null;

  /* -- small shared pieces ------------------------------------------ */

  const VIEWS = ["home", "page", "editor", "new"];

  function showView(name) {
    for (const view of VIEWS) els.views[view].hidden = view !== name;
  }

  /** A plain button with a handler, for the notice area's actions. */
  function mkButton(label, handler) {
    const button = document.createElement("button");
    button.type = "button";
    button.textContent = label;
    button.addEventListener("click", handler);
    return button;
  }

  /** The signed-out message: text plus the link to the sign-in page. */
  function signInNotice(message) {
    notice(els.notice, message, "warn");
    const link = document.createElement("a");
    link.href = "index.html";
    link.textContent = "Sign in";
    els.notice.appendChild(link);
  }

  function clearNotice() {
    notice(els.notice, "", "ok");
  }

  /* -- header session bar + drawer mirrors -------------------------- */

  function paintSession() {
    applyHeader(
      {
        bar: $("#session-bar"),
        who: $("#session-who"),
        signin: $("#header-signin"),
      },
      state.session,
    );
    if (els.drawerSignin) els.drawerSignin.hidden = state.signedIn;
    if (els.drawerSignout) els.drawerSignout.hidden = !state.signedIn;
    // Ask hits the API like Edit and Pin do, so its two buttons stand down
    // with the write affordances wherever they live (page toolbar, editor).
    for (const button of [els.edit, els.pinToggle, els.askOpen, els.askOpenEdit]) {
      if (button) button.hidden = !state.signedIn;
    }
    paintNewPage();
  }

  /** The New page affordances are write affordances, exactly like Edit: they
   * hide signed out, wherever they appear — the home toolbar, the empty
   * state and the page-view toolbar. */
  function paintNewPage() {
    for (const trigger of $$("[data-new-page]")) trigger.hidden = !state.signedIn;
  }

  function openDrawer() {
    els.drawer.hidden = false;
    els.drawerToggle.setAttribute("aria-expanded", "true");
    const first = els.drawer.querySelector("a");
    if (first && typeof first.focus === "function") first.focus();
  }

  function closeDrawer() {
    els.drawer.hidden = true;
    els.drawerToggle.setAttribute("aria-expanded", "false");
  }

  /* -- home ---------------------------------------------------------- */

  function rowElement(row, pinnable) {
    const li = document.createElement("li");
    li.className = "wiki-row";
    const link = document.createElement("a");
    link.className = "wiki-row__link";
    link.href = `wiki.html?slug=${encodeURIComponent(row.slug || "")}`;
    const slug = document.createElement("span");
    slug.className = "wiki-row__slug";
    slug.textContent = row.slug || "(unnamed)";
    const pill = document.createElement("span");
    pill.className = "pill";
    pill.textContent = row.entity_type || "page";
    const meta = document.createElement("span");
    meta.className = "wiki-row__meta";
    meta.textContent = [formatDate(row.updated_at), row.version != null ? `v${row.version}` : ""]
      .filter(Boolean)
      .join(" · ");
    link.append(slug, pill, meta);
    li.appendChild(link);
    if (pinnable) {
      const unpin = mkButton("Unpin", () => {
        writePins(readPins().filter((id) => id !== pinId(row)));
        renderHome();
      });
      unpin.className = "wiki-row__pin";
      unpin.setAttribute("aria-label", `Unpin ${row.slug || "page"}`);
      li.appendChild(unpin);
    }
    return li;
  }

  function fillList(list, rows, pinnable) {
    list.replaceChildren();
    for (const row of rows) list.appendChild(rowElement(row, pinnable));
  }

  function renderHome() {
    const query = state.filter.trim().toLowerCase();
    const matches = state.rows.filter(
      (row) =>
        !query ||
        `${row.slug || ""} ${row.entity_type || ""}`.toLowerCase().includes(query),
    );
    const pins = readPins();
    const pinned = matches.filter((row) => pins.includes(pinId(row)));
    const recent = matches.filter((row) => !pins.includes(pinId(row)));
    fillList(els.pinnedList, pinned, true);
    fillList(els.recentList, recent, false);
    els.pinnedSection.hidden = pinned.length === 0;
    els.recentSection.hidden = recent.length === 0;
    els.listFilter.hidden = state.rows.length === 0;
    els.homeEmpty.hidden = state.rows.length !== 0;
    els.homeNomatch.hidden = !(state.rows.length > 0 && matches.length === 0);
  }

  function renderHomeSignedOut() {
    els.listFilter.hidden = true;
    els.pinnedSection.hidden = true;
    els.recentSection.hidden = true;
    els.homeNomatch.hidden = true;
    els.homeEmpty.hidden = true;
    els.homeStatus.textContent = "";
    signInNotice("Sign in to see the wiki.");
  }

  async function loadHome() {
    showView("home");
    if (!state.signedIn) {
      renderHomeSignedOut();
      return;
    }
    try {
      const data = await api("/v1/pages?limit=200");
      state.rows = data && Array.isArray(data.pages) ? data.pages : [];
      els.homeStatus.textContent = `${state.rows.length} ${
        state.rows.length === 1 ? "page" : "pages"
      }`;
      renderHome();
      clearNotice();
    } catch (error) {
      if (error instanceof ApiError && error.status === 401) {
        state.signedIn = false;
        paintSession();
        renderHomeSignedOut();
        return;
      }
      renderHomeSignedOut();
      notice(els.notice, `Could not load pages: ${error.message}`, "bad");
      els.notice.appendChild(mkButton("Retry", loadHome));
    }
  }

  /* -- page view ----------------------------------------------------- */

  function currentPinId() {
    return state.page ? pinId(state.page) : "";
  }

  function paintPin() {
    const pinned = readPins().includes(currentPinId());
    els.pinToggle.textContent = pinned ? "Unpin" : "Pin";
    els.pinToggle.setAttribute("aria-pressed", String(pinned));
  }

  function togglePin() {
    const id = currentPinId();
    if (!id || id.endsWith("/")) return;
    const pins = readPins();
    const at = pins.indexOf(id);
    if (at === -1) pins.unshift(id);
    else pins.splice(at, 1);
    writePins(pins);
    paintPin();
  }

  /** Heading ids (slugged, deduped `-2`, `-3`…), then a TOC of the h2/h3s
   * into both mounts — the desktop aside and the drawer. */
  function wireHeadingsAndToc() {
    const used = new Map();
    const entries = [];
    for (const heading of $$("h1, h2, h3, h4, h5, h6", els.pageBody)) {
      const base = slugify(heading.textContent) || "section";
      const seen = used.get(base) || 0;
      used.set(base, seen + 1);
      const id = seen ? `${base}-${seen + 1}` : base;
      heading.setAttribute("id", id);
      entries.push({
        id,
        level: String(heading.tagName || heading.name || "h6").toLowerCase(),
        text: heading.textContent,
      });
    }
    const toc = entries.filter((e) => e.level === "h2" || e.level === "h3");
    const description = el(
      "ul",
      toc.map((entry) =>
        el(
          "li",
          [
            el("a", [tx(entry.text)], {
              href: `#${entry.id}`,
            }),
          ],
          entry.level === "h3" ? { class: "wiki-toc__sub" } : null,
        ),
      ),
    );
    renderInto(els.tocList, description);
    renderInto(els.tocDrawer, description);
    const enough = toc.length >= 2;
    els.tocDesktop.hidden = !enough;
    if (els.tocDrawerHeading) els.tocDrawerHeading.hidden = !enough;
  }

  /** The backlinks and citations cards: each is a plain `section.card` of
   * heading, blurb and one list, so a card whose list came back empty — a
   * legitimately linkless page, an error, a signed-out visit — is all heading
   * and no body. The whole card stands down then, signed in or not. */
  function paintCards(backlinks, citations) {
    for (const [content, empty] of [
      [els.pageBacklinks, backlinks.length === 0],
      [els.pageCitations, citations.length === 0],
    ]) {
      content.hidden = empty;
      const card = content.parentNode;
      if (card) card.hidden = empty;
    }
  }

  function clearArticle() {
    renderInto(els.pageBody, []);
    renderInto(els.pageBacklinks, []);
    renderInto(els.pageCitations, []);
    els.pageMeta.replaceChildren();
    paintCards([], []);
    wireHeadingsAndToc();
  }

  /** The signed-out page shell: the title (the slug) and the sign-in notice,
   * nothing else — no article, no meta, no toolbar, no empty cards. */
  function paintSignedOutPage() {
    els.pageBody.hidden = true;
    els.pageMeta.hidden = true;
    clearArticle();
    signInNotice("Sign in to read this page.");
  }

  /**
   * Replaces every `[[slug]]` / `[[slug|Label]]` in the article's text with a
   * real anchor: a `TreeWalker` over the text nodes, each swapped in place
   * for text nodes and anchors built with `createElement` + `textContent`,
   * like everything else that reaches the document. A body that never yields
   * a valid slug — `[[orbit window]]` bare, say — keeps the literal brackets
   * it was written with, and `pre`/`code` are never touched: there `[[…]]` is
   * quoted text, not a link.
   */
  function linkWikilinks(container) {
    if (!container || typeof document.createTreeWalker !== "function") return;
    const walker = document.createTreeWalker(container, SHOW_TEXT);
    const nodes = [];
    while (walker.nextNode()) nodes.push(walker.currentNode);
    const pattern = /\[\[([^[\]]+)\]\]/g;
    for (const node of nodes) {
      const owner = node.parentNode;
      const value = String(
        node.nodeValue != null ? node.nodeValue : node.textContent || "",
      );
      if (!owner || !value.includes("[[")) continue;
      if (typeof owner.closest === "function" && owner.closest("pre, code")) {
        continue;
      }
      const parts = [];
      let cursor = 0;
      let match;
      while ((match = pattern.exec(value)) !== null) {
        const link = wikilinkToLink(match[1]);
        if (!link) continue;
        if (match.index > cursor) {
          parts.push(document.createTextNode(value.slice(cursor, match.index)));
        }
        const anchor = document.createElement("a");
        anchor.setAttribute(
          "href",
          `wiki.html?slug=${encodeURIComponent(link.slug)}`,
        );
        anchor.textContent = link.label;
        parts.push(anchor);
        cursor = match.index + match[0].length;
      }
      if (!parts.length) continue;
      if (cursor < value.length) {
        parts.push(document.createTextNode(value.slice(cursor)));
      }
      node.replaceWith(...parts);
    }
  }

  function renderPage() {
    const page = state.page || {};
    els.pageBody.hidden = false;
    els.pageMeta.hidden = false;
    els.pageTitle.textContent = page.title || page.slug || "Untitled";
    const pill = document.createElement("span");
    pill.className = "pill";
    pill.textContent = page.entity_type || "page";
    els.pageMeta.replaceChildren();
    els.pageMeta.appendChild(pill);
    const where = parseBrainUrl(page.url);
    const facts = [];
    if (page.version != null) facts.push(`v${page.version}`);
    if (where && where.scope) facts.push(where.scope);
    if (facts.length) els.pageMeta.appendChild(document.createTextNode(facts.join(" · ")));
    renderInto(els.pageBody, renderMarkdown(stripFrontmatter(page.markdown || "")));
    linkWikilinks(els.pageBody);
    wireHeadingsAndToc();
    const backlinks = Array.isArray(page.backlinks) ? page.backlinks : [];
    const citations = Array.isArray(page.citations) ? page.citations : [];
    renderInto(els.pageBacklinks, renderBacklinks(backlinks));
    renderInto(els.pageCitations, renderCitations(citations));
    paintCards(backlinks, citations);
    paintPin();
  }

  async function openPage(targetSlug, { edit = false } = {}) {
    state.slug = targetSlug;
    state.page = null;
    state.version = null;
    showView("page");
    if (!state.signedIn) {
      els.pageTitle.textContent = targetSlug;
      paintSignedOutPage();
      return;
    }
    let page;
    try {
      page = await api(`/v1/pages/${encodeURIComponent(targetSlug)}`);
    } catch (error) {
      els.pageTitle.textContent = targetSlug;
      els.pageBody.hidden = true;
      els.pageMeta.hidden = true;
      clearArticle();
      els.edit.hidden = true;
      els.pinToggle.hidden = true;
      els.askOpen.hidden = true;
      if (error instanceof ApiError && error.status === 404) {
        notice(
          els.notice,
          "No page has this slug yet — the New page button creates it.",
          "warn",
        );
        return;
      }
      if (error instanceof ApiError && error.status === 401) {
        state.signedIn = false;
        paintSession();
        paintSignedOutPage();
        return;
      }
      notice(els.notice, `Could not open ${targetSlug}: ${error.message}`, "bad");
      return;
    }
    state.page = page;
    state.version = page.version == null ? null : page.version;
    if (edit) openEditor();
    else {
      renderPage();
      clearNotice();
    }
  }

  /* -- editor ---------------------------------------------------------- */

  function paintEditMode() {
    els.source.hidden = state.previewing;
    els.preview.hidden = !state.previewing;
    els.editWrite.setAttribute("aria-pressed", String(!state.previewing));
    els.editPreview.setAttribute("aria-pressed", String(state.previewing));
  }

  /** The preview pane: always the textarea's current content, but stripped of
   * its frontmatter fence — a display, like the read view, never the source. */
  function renderPreview() {
    renderInto(els.preview, renderMarkdown(stripFrontmatter(els.source.value)));
  }

  function openEditor() {
    const page = state.page || {};
    els.editTitle.value = page.title || "";
    els.source.value = page.markdown || "";
    state.previewing = false;
    renderPreview();
    paintEditMode();
    els.status.textContent = "";
    clearNotice();
    showView("editor");
    if (typeof els.editTitle.focus === "function") els.editTitle.focus();
  }

  /** Lands the editor's result back on the read view, from the server's
   * response — the store redacts secrets before saving, so what it returns,
   * not what was sent, is the page's truth. */
  function applySave(saved, sent) {
    state.page = saved;
    if (saved && saved.version != null) state.version = saved.version;
    if (saved && saved.slug) state.slug = saved.slug;
    if (saved && saved.markdown != null) els.source.value = saved.markdown;
    if (state.slug) {
      history.replaceState(
        null,
        "",
        `${location.pathname}?slug=${encodeURIComponent(state.slug)}`,
      );
    }
    renderPage();
    showView("page");
    const redacted = Boolean(saved && saved.markdown != null && saved.markdown !== sent);
    notice(
      els.notice,
      redacted ? "Sensitive-looking text was redacted before saving." : "Saved.",
      redacted ? "warn" : "ok",
    );
  }

  async function savePage() {
    if (!state.slug) return;
    const sent = els.source.value;
    els.save.disabled = true;
    try {
      const saved = await api(`/v1/pages/${encodeURIComponent(state.slug)}`, {
        method: "PUT",
        body: {
          markdown: sent,
          base_version: state.version,
          title: els.editTitle.value.trim(),
        },
      });
      applySave(saved, sent);
    } catch (error) {
      if (error instanceof ApiError && error.status === 409) showConflict();
      else notice(els.notice, `Not saved: ${error.message}`, "bad");
    } finally {
      els.save.disabled = false;
    }
  }

  /** The optimistic-concurrency refusal: keep the draft, offer the two ways
   * out as buttons on the notice itself. */
  function showConflict() {
    notice(
      els.notice,
      "Someone else saved this page while you were editing. Your text is " +
        "still here — load their version, or overwrite it with yours.",
      "bad",
    );
    els.notice.appendChild(mkButton("Load their version", loadTheirs));
    els.notice.appendChild(mkButton("Overwrite with mine", overwriteMine));
  }

  async function loadTheirs() {
    try {
      const fresh = await api(`/v1/pages/${encodeURIComponent(state.slug)}`);
      state.page = fresh;
      state.version = fresh.version == null ? null : fresh.version;
      els.source.value = fresh.markdown || "";
      els.editTitle.value = fresh.title || "";
      renderPreview();
      notice(els.notice, "Loaded the current version — your draft was replaced.", "warn");
    } catch (error) {
      notice(els.notice, `Could not load the current version: ${error.message}`, "bad");
    }
  }

  async function overwriteMine() {
    const sent = els.source.value;
    try {
      const fresh = await api(`/v1/pages/${encodeURIComponent(state.slug)}`);
      const saved = await api(`/v1/pages/${encodeURIComponent(state.slug)}`, {
        method: "PUT",
        body: {
          markdown: sent,
          base_version: fresh.version,
          title: els.editTitle.value.trim(),
        },
      });
      applySave(saved, sent);
    } catch (error) {
      notice(els.notice, `Not saved: ${error.message}`, "bad");
    }
  }

  /** Back out of the editor or the new-page form to wherever the page was. */
  function leaveEditing() {
    clearNotice();
    if (state.page) {
      renderPage();
      showView("page");
    } else {
      loadHome();
    }
  }

  /* -- new page -------------------------------------------------------- */

  function paintNpValidity() {
    const value = els.npSlug.value;
    const ok = validSlug(value);
    els.npCreate.disabled = !ok;
    els.npSlugError.textContent = ok ? "" : SLUG_MESSAGE;
    // `aria-invalid` is also the cue wiki.css uses to turn the slot
    // danger-red, so it goes on only once validation has actually failed: a
    // slug was typed and refused. A pristine (blank) field keeps the rule as
    // neutral helper text — the disabled Create button already says the form
    // is not ready.
    if (ok || value === "") els.npSlug.removeAttribute("aria-invalid");
    else els.npSlug.setAttribute("aria-invalid", "true");
  }

  function openNewPage() {
    els.npSlug.value = "";
    els.npTitle.value = "";
    paintNpValidity();
    showView("new");
    clearNotice();
    if (typeof els.npSlug.focus === "function") els.npSlug.focus();
  }

  async function createPage(event) {
    if (event && typeof event.preventDefault === "function") event.preventDefault();
    const slug = els.npSlug.value;
    if (!validSlug(slug)) {
      paintNpValidity();
      if (typeof els.npSlug.focus === "function") els.npSlug.focus();
      return;
    }
    els.npCreate.disabled = true;
    try {
      const saved = await api(`/v1/pages/${encodeURIComponent(slug)}`, {
        method: "PUT",
        body: { markdown: "", base_version: null, title: els.npTitle.value.trim() },
      });
      state.page = saved;
      state.version = saved && saved.version != null ? saved.version : null;
      state.slug = (saved && saved.slug) || slug;
      history.replaceState(
        null,
        "",
        `${location.pathname}?slug=${encodeURIComponent(state.slug)}`,
      );
      renderPage();
      showView("page");
      notice(els.notice, "Created.", "ok");
    } catch (error) {
      if (error instanceof ApiError && error.status === 401) {
        state.signedIn = false;
        paintSession();
        signInNotice("Sign in to create a page.");
      } else {
        els.npSlugError.textContent = error.message;
        els.npSlug.setAttribute("aria-invalid", "true");
      }
    } finally {
      // Re-arm the button from the slug's validity alone — a full repaint
      // here would wipe a server error just written under the field.
      els.npCreate.disabled = !validSlug(els.npSlug.value);
    }
  }

  /* -- ask --------------------------------------------------------------- */

  function openAsk() {
    const page = state.page || {};
    const subject = page.title || page.slug || "the wiki";
    els.askQuestion.value = `What is "${subject}" about?`;
    els.askStatus.textContent = "";
    renderInto(els.askAnswer, []);
    renderInto(els.askCitations, []);
    if (typeof els.askPanel.showModal === "function") els.askPanel.showModal();
    else els.askPanel.hidden = false;
    if (typeof els.askQuestion.focus === "function") els.askQuestion.focus();
  }

  function closeAsk() {
    if (typeof els.askPanel.close === "function") els.askPanel.close();
    else els.askPanel.hidden = true;
  }

  /** Ask citations: the title as a link when its URL is safe, the quoted line
   * under it as a blockquote. */
  function askCitations(list) {
    const items = (Array.isArray(list) ? list : []).map((raw) => {
      const citation = raw || {};
      const title = String(citation.title == null ? "Untitled source" : citation.title);
      const url = String(citation.url == null ? "" : citation.url).trim();
      const quote = String(citation.quote == null ? "" : citation.quote).trim();
      const children = [
        isSafeHref(url)
          ? el("a", [tx(title)], { href: url, rel: "noopener noreferrer" })
          : el("span", [tx(title)]),
      ];
      if (quote) children.push(el("blockquote", [tx(quote)]));
      return el("li", children);
    });
    return el("ul", items, { class: "citations" });
  }

  async function submitAsk() {
    const question = els.askQuestion.value.trim();
    if (!question) {
      els.askStatus.textContent = "Type a question first.";
      return;
    }
    const label = els.askSubmit.textContent;
    els.askSubmit.disabled = true;
    els.askSubmit.textContent = "Asking…";
    els.askStatus.textContent = "";
    try {
      const answer = await api("/v1/ask", { method: "POST", body: { question } });
      renderInto(els.askAnswer, renderMarkdown((answer && answer.answer) || ""));
      renderInto(els.askCitations, askCitations(answer && answer.citations));
    } catch (error) {
      els.askStatus.textContent = `Could not ask: ${error.message}`;
    } finally {
      els.askSubmit.disabled = false;
      els.askSubmit.textContent = label;
    }
  }

  /* -- header search ------------------------------------------------------ */

  function setActiveSearch(index) {
    state.activeSearch = index;
    state.searchOptions.forEach((option, at) => {
      option.className =
        at === index ? "wiki-search__opt wiki-search__opt--active" : "wiki-search__opt";
      option.setAttribute("aria-selected", String(at === index));
    });
    const active = state.searchOptions[index];
    if (active) {
      const id =
        typeof active.getAttribute === "function"
          ? active.getAttribute("id")
          : active.attrs.id;
      els.searchInput.setAttribute("aria-activedescendant", id);
      if (typeof active.scrollIntoView === "function") {
        active.scrollIntoView({ block: "nearest" });
      }
    } else if (typeof els.searchInput.removeAttribute === "function") {
      els.searchInput.removeAttribute("aria-activedescendant");
    }
  }

  function closeSearch() {
    els.searchResults.hidden = true;
    els.searchResults.replaceChildren();
    state.searchResults = [];
    state.searchOptions = [];
    setActiveSearch(-1);
    els.searchInput.setAttribute("aria-expanded", "false");
  }

  /** Opens the result the keyboard (or a click) chose. The server's result
   * URLs are site URLs; only one that parses back to a slug is followable. */
  function chooseResult(index) {
    const item = state.searchResults[index];
    closeSearch();
    if (!item) return;
    const where = parseBrainUrl(item.url);
    if (where && where.slug) {
      location.assign(`wiki.html?slug=${encodeURIComponent(where.slug)}`);
    }
  }

  function paintSearchMessage(message) {
    els.searchResults.replaceChildren();
    const line = document.createElement("p");
    line.className = "wiki-search__none";
    line.textContent = message;
    els.searchResults.appendChild(line);
    els.searchResults.hidden = false;
    els.searchInput.setAttribute("aria-expanded", "true");
  }

  async function runSearch(query) {
    let data;
    try {
      data = await api(
        `/v1/search?q=${encodeURIComponent(query)}&limit=${SEARCH_LIMIT}`,
      );
    } catch (error) {
      if (error instanceof ApiError && error.status === 401) paintSearchMessage("Sign in to search.");
      else closeSearch();
      return;
    }
    if (els.searchInput.value.trim() !== query) return; // a newer keystroke won
    const results = data && Array.isArray(data.results) ? data.results : [];
    state.searchResults = results;
    state.searchOptions = [];
    els.searchResults.replaceChildren();
    if (!results.length) {
      paintSearchMessage("No results.");
      return;
    }
    const words = terms(query);
    results.forEach((item, at) => {
      const option = document.createElement("div");
      option.className = "wiki-search__opt";
      option.setAttribute("role", "option");
      option.setAttribute("id", `wiki-search-opt-${at}`);
      option.setAttribute("aria-selected", "false");
      const title = document.createElement("span");
      title.className = "wiki-search__title";
      mountNodes(firstHighlight(String((item && item.title) || ""), words), title);
      const snippet = document.createElement("span");
      snippet.className = "wiki-search__snippet";
      mountNodes(highlightTerms(String((item && item.snippet) || ""), words), snippet);
      option.append(title, snippet);
      option.addEventListener("click", () => chooseResult(at));
      els.searchResults.appendChild(option);
      state.searchOptions.push(option);
    });
    els.searchResults.hidden = false;
    els.searchInput.setAttribute("aria-expanded", "true");
    setActiveSearch(-1);
  }

  /* -- wire everything ----------------------------------------------------- */

  els.drawerToggle.addEventListener("click", () =>
    els.drawer.hidden ? openDrawer() : closeDrawer(),
  );

  if (els.drawerSignout) {
    els.drawerSignout.addEventListener("click", async () => {
      try {
        const request = signOutRequest();
        await api(request.path, { method: request.method });
      } catch {
        /* the cookie may already be gone; either way, leave for the front door */
      }
      location.assign("index.html");
    });
  }

  for (const trigger of $$("[data-new-page]")) {
    trigger.addEventListener("click", openNewPage);
  }

  els.listFilter.addEventListener("input", () => {
    state.filter = els.listFilter.value;
    renderHome();
  });

  els.edit.addEventListener("click", openEditor);
  els.cancelEdit.addEventListener("click", leaveEditing);
  els.save.addEventListener("click", savePage);
  els.pinToggle.addEventListener("click", togglePin);

  els.editWrite.addEventListener("click", () => {
    state.previewing = false;
    paintEditMode();
  });
  els.editPreview.addEventListener("click", () => {
    state.previewing = true;
    // The preview is always the textarea's current content.
    renderPreview();
    paintEditMode();
  });

  els.npSlug.addEventListener("input", paintNpValidity);
  els.npForm.addEventListener("submit", createPage);
  els.npCancel.addEventListener("click", leaveEditing);

  els.askOpen.addEventListener("click", openAsk);
  els.askOpenEdit.addEventListener("click", openAsk);
  els.askClose.addEventListener("click", closeAsk);
  els.askSubmit.addEventListener("click", submitAsk);

  els.searchInput.addEventListener("input", () => {
    clearTimeout(searchTimer);
    const query = els.searchInput.value.trim();
    if (query.length < 2) {
      closeSearch();
      return;
    }
    searchTimer = setTimeout(() => runSearch(query), SEARCH_DEBOUNCE_MS);
  });
  els.searchInput.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      closeSearch();
      return;
    }
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      const count = state.searchOptions.length;
      if (!count) return;
      event.preventDefault();
      let next = state.activeSearch + (event.key === "ArrowDown" ? 1 : -1);
      if (next < -1) next = count - 1;
      if (next >= count) next = -1;
      setActiveSearch(next);
      return;
    }
    if (event.key === "Enter") {
      event.preventDefault();
      chooseResult(state.activeSearch >= 0 ? state.activeSearch : 0);
    }
  });
  els.searchForm.addEventListener("submit", (event) => {
    event.preventDefault();
    if (state.searchResults.length) {
      chooseResult(state.activeSearch >= 0 ? state.activeSearch : 0);
    }
  });

  // One document keydown for the two global keys: "/" focuses search (unless
  // the keystroke is aimed at a field), Escape shuts the drawer.
  document.addEventListener("keydown", (event) => {
    if (!event || typeof event.key !== "string") return;
    if (event.key === "/" && !event.metaKey && !event.ctrlKey && !event.altKey) {
      const target = event.target;
      const tag =
        target && typeof target.tagName === "string" ? target.tagName.toLowerCase() : "";
      const typing =
        tag === "input" || tag === "textarea" || tag === "select" ||
        Boolean(target && target.isContentEditable);
      if (!typing) {
        event.preventDefault();
        els.searchInput.focus();
      }
      return;
    }
    if (event.key === "Escape" && !els.drawer.hidden) closeDrawer();
  });

  // Clicks outside the search panel or the drawer close them.
  document.addEventListener("click", (event) => {
    const target = event && event.target;
    const inside = (sel) =>
      Boolean(target && typeof target.closest === "function" && target.closest(sel));
    if (!els.searchResults.hidden && !inside("#wiki-search-form")) closeSearch();
    if (!els.drawer.hidden && !inside("#wiki-drawer") && !inside("#drawer-toggle")) {
      closeDrawer();
    }
  });

  /* -- boot: session, then route ------------------------------------------ */

  state.session = await checkSession(api);
  state.signedIn = state.session.kind === "signed-in";
  paintSession();

  if (startSlug) await openPage(startSlug, { edit: startInEdit });
  else await loadHome();
}
