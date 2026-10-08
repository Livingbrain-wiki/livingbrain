// The pure half of the wiki renderer.
//
// `renderMarkdown`, `renderCitations` and `renderBacklinks` return *node
// descriptions*, never HTML strings. The DOM is built from these by
// `mountNodes` in `app.js`, which creates elements and sets `textContent` —
// so a page body cannot become markup, because no step in this pipeline ever
// parses a string as HTML. Everything here is a pure function of its inputs,
// which is what lets `node --test app/tests/` exercise exactly the code the
// page runs.

/** A text run: the only leaf kind that ever carries user content. */
function text(value) {
  return { type: "text", value: String(value) };
}

function element(tag, children, attrs) {
  const node = { type: "element", tag, children: children || [] };
  if (attrs && Object.keys(attrs).length) node.attrs = attrs;
  return node;
}

/**
 * The URL schemes a link may use. Everything else — `javascript:`,
 * `data:`, `vbscript:` — is rendered as plain text rather than dropped, so a
 * person reading the page sees what was written without being able to click
 * it into execution.
 */
const SAFE_SCHEMES = ["http:", "https:", "mailto:"];

/**
 * True when `href` is safe to put in an anchor's `href`.
 * Relative and protocol-relative links are allowed; an absolute URL must name
 * a scheme on the allow list.
 */
export function isSafeHref(href) {
  const value = String(href == null ? "" : href).trim();
  if (!value) return false;
  // Browsers strip control characters before parsing a scheme, so
  // `java\nscript:` has to be judged as the `javascript:` it becomes.
  const flat = value.replace(/[\u0000-\u001f\u007f]/g, "").toLowerCase();
  if (/^[a-z][a-z0-9+.-]*:/.test(flat)) {
    return SAFE_SCHEMES.some((scheme) => flat.startsWith(scheme));
  }
  // No scheme at all: a relative path, a fragment, or a protocol-relative
  // URL. It inherits the page's scheme and cannot name a script handler, so
  // it is as safe as this page is.
  return true;
}

/**
 * Splits inline Markdown into runs: code spans, strong, emphasis and links.
 * Exported for the tests; the page only calls `renderMarkdown`.
 */
export function parseInline(source) {
  const runs = [];
  // Fenced code first: nothing inside a code span is markup, and the URL
  // rules do not apply to it.
  const code = /`([^`]+)`/g;
  let cursor = 0;
  let match;
  const push = (value) => {
    if (value) runs.push(text(value));
  };
  while ((match = code.exec(source)) !== null) {
    push(source.slice(cursor, match.index));
    runs.push(element("code", [text(match[1])]));
    cursor = match.index + match[0].length;
  }
  push(source.slice(cursor));
  // Both passes return a list per input run, so each is flattened.
  return runs.flatMap(expandEmphasis).flatMap(expandLinks);
}

/** Wraps `**…**` and `*…*` spans in strong and em elements. */
function expandEmphasis(run) {
  if (run.type !== "text") return [run];
  const out = [];
  const pattern = /\*\*([^*]+)\*\*|\*([^*]+)\*/g;
  let cursor = 0;
  let match;
  while ((match = pattern.exec(run.value)) !== null) {
    if (match.index > cursor) out.push(text(run.value.slice(cursor, match.index)));
    const body = match[1] != null ? match[1] : match[2];
    out.push(element(match[1] != null ? "strong" : "em", [text(body)]));
    cursor = match.index + match[0].length;
  }
  if (cursor === 0) return [run];
  if (cursor < run.value.length) out.push(text(run.value.slice(cursor)));
  return out;
}

/** Turns `[label](url)` into an anchor, or into plain text when the URL is unsafe. */
function expandLinks(run) {
  if (run.type !== "text") return [run];
  const pattern = /\[([^\]]+)\]\(([^)\s]+)\)/g;
  const out = [];
  let cursor = 0;
  let match;
  while ((match = pattern.exec(run.value)) !== null) {
    if (match.index > cursor) out.push(text(run.value.slice(cursor, match.index)));
    const label = match[1];
    const href = match[2];
    if (isSafeHref(href)) {
      const attrs = { href };
      // `target="_blank"` without this leaks the referring page.
      if (/^https?:/i.test(href)) attrs.rel = "noopener noreferrer";
      out.push(element("a", [text(label)], attrs));
    } else {
      // Unsafe URL: keep the visible words, drop the ability to click them.
      out.push(text(label));
    }
    cursor = match.index + match[0].length;
  }
  if (cursor === 0) return [run];
  if (cursor < run.value.length) out.push(text(run.value.slice(cursor)));
  return out;
}

/**
 * Markdown source to a list of block node descriptions.
 *
 * Supports what a wiki page actually uses: ATX headings, fenced code, block
 * quotes, unordered and ordered lists, and paragraphs. Anything else — raw
 * HTML in the source, for instance — is text, because the only leaf kind is
 * `text` and the only way a tag is emitted is by this function naming it.
 */
export function renderMarkdown(markdown) {
  const source = String(markdown == null ? "" : markdown).replace(/\r\n?/g, "\n");
  const lines = source.split("\n");
  const blocks = [];
  let i = 0;

  const flushParagraph = (buffer) => {
    const body = buffer.join("\n").trim();
    if (body) blocks.push(element("p", parseInline(body)));
    buffer.length = 0;
  };

  const paragraph = [];

  while (i < lines.length) {
    const line = lines[i];

    if (!line.trim()) {
      flushParagraph(paragraph);
      i += 1;
      continue;
    }

    // Fenced code: kept verbatim, still escaped because it is a text leaf.
    const fence = /^\s*```\s*([\w-]*)\s*$/.exec(line);
    if (fence) {
      flushParagraph(paragraph);
      const body = [];
      i += 1;
      while (i < lines.length && !/^\s*```\s*$/.test(lines[i])) {
        body.push(lines[i]);
        i += 1;
      }
      i += 1; // closing fence, or end of input
      const pre = element("pre", [element("code", [text(body.join("\n"))])]);
      if (fence[1]) pre.attrs = { "data-lang": fence[1] };
      blocks.push(pre);
      continue;
    }

    const heading = /^(#{1,6})\s+(.*)$/.exec(line);
    if (heading) {
      flushParagraph(paragraph);
      const level = heading[1].length;
      blocks.push(element(`h${level}`, parseInline(heading[2].trim())));
      i += 1;
      continue;
    }

    if (/^\s*(?:---+|\*\*\*+)\s*$/.test(line)) {
      flushParagraph(paragraph);
      blocks.push(element("hr", []));
      i += 1;
      continue;
    }

    const quote = /^\s*>\s?(.*)$/.exec(line);
    if (quote) {
      flushParagraph(paragraph);
      const body = [quote[1]];
      i += 1;
      while (i < lines.length && /^\s*>\s?(.*)$/.test(lines[i])) {
        body.push(/^\s*>\s?(.*)$/.exec(lines[i])[1]);
        i += 1;
      }
      blocks.push(element("blockquote", parseInline(body.join("\n").trim())));
      continue;
    }

    const bullet = /^\s*[-*+]\s+(.*)$/.exec(line);
    const ordered = /^\s*\d+[.)]\s+(.*)$/.exec(line);
    if (bullet || ordered) {
      flushParagraph(paragraph);
      const tag = bullet ? "ul" : "ol";
      const items = [];
      const itemPattern = bullet
        ? /^\s*[-*+]\s+(.*)$/
        : /^\s*\d+[.)]\s+(.*)$/;
      while (i < lines.length) {
        const item = itemPattern.exec(lines[i]);
        if (!item) break;
        const body = [item[1]];
        i += 1;
        // A continuation line (indented, not a new item) stays in the item.
        while (
          i < lines.length &&
          /^\s{2,}\S/.test(lines[i]) &&
          !itemPattern.test(lines[i])
        ) {
          body.push(lines[i].trim());
          i += 1;
        }
        items.push(element("li", parseInline(body.join("\n").trim())));
      }
      blocks.push(element(tag, items));
      continue;
    }

    paragraph.push(line);
    i += 1;
  }

  flushParagraph(paragraph);
  return blocks;
}

/**
 * One citation as a node description: the title as a link when there is a URL,
 * the quote when there is one, and the URL as text either way.
 *
 * A citation with a URL is the normal case; a citation without one still has
 * to render, because a source that has no address is still a claim's source.
 */
export function renderCitation(citation) {
  const c = citation || {};
  const title = String(c.title == null ? "Untitled source" : c.title);
  const url = String(c.url == null ? "" : c.url).trim();
  const quote = String(c.quote == null ? "" : c.quote).trim();

  const head = isSafeHref(url)
    ? element("a", [text(title)], { href: url, rel: "noopener noreferrer" })
    : element("span", [text(title)]);

  const children = [head];
  if (quote) children.push(element("span", [text(quote)], { class: "quote" }));
  if (url) {
    // The URL is shown as text too, so a reader can see where a claim came
    // from without clicking and without the text being unescaped markup.
    children.push(element("span", [text(url)], { class: "tiny" }));
  }
  return element("li", children);
}

/** A list of citations, or an empty `ul` when there are none. */
export function renderCitations(citations) {
  const list = Array.isArray(citations) ? citations : [];
  return element(
    "ul",
    list.map(renderCitation),
    list.length ? { class: "citations" } : { class: "citations", hidden: "" },
  );
}

/**
 * Backlinks as node descriptions. The input is `Vec<String>` of slugs; a
 * blank or duplicated slug is dropped, because the server sends what it has
 * and the page should not repeat an entry.
 */
export function renderBacklinks(slugs) {
  const seen = new Set();
  const items = [];
  for (const raw of Array.isArray(slugs) ? slugs : []) {
    const slug = String(raw == null ? "" : raw).trim();
    if (!slug || seen.has(slug)) continue;
    seen.add(slug);
    items.push(
      element("li", [
        element("a", [text(slug)], { href: `wiki.html?slug=${encodeURIComponent(slug)}` }),
      ]),
    );
  }
  return element(
    "ul",
    items,
    items.length ? { class: "backlinks" } : { class: "backlinks", hidden: "" },
  );
}